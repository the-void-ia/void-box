//! VzBackend: [`VmmBackend`] implementation using Apple's Virtualization.framework.
//!
//! ## Lifecycle
//!
//! 1. `start()`: Configures and boots a `VZVirtualMachine` with:
//!    - `VZLinuxBootLoader` (kernel, initrd, cmdline)
//!    - `VZVirtioSocketDeviceConfiguration` (for host↔guest control channel)
//!    - `VZNATNetworkDeviceAttachment` (if networking enabled)
//!    - `VZVirtioFileSystemDeviceConfiguration` (if shared_dir provided)
//!
//!    If `config.snapshot` is `Some`, restores from a VZ snapshot instead of cold-booting.
//! 2. `exec()`, `write_file()`, etc.: Delegate to `ControlChannel` over vsock fd
//! 3. `stop()`: Requests VM stop via Virtualization.framework
//!
//! ## Snapshot/Restore
//!
//! Uses Apple's native `saveMachineStateToURL:` / `restoreMachineStateFromURL:`
//! APIs (macOS 14+). These handle all CPU + memory state internally. A small
//! JSON sidecar (`vz_meta.json`) persists our metadata (session_secret, config)
//! alongside Apple's opaque save file.
//!
//! ## Network Security (v1 limitation)
//!
//! VZ provides `VZNATNetworkDeviceAttachment` which gives NAT networking out of
//! the box. However, unlike Linux/KVM where the SLIRP stack enforces CIDR deny
//! lists, rate limiting, and connection counting at the host level, VZ NAT does
//! **not** provide these controls.
//!
//! **v1**: CIDR deny lists are enforced in-guest via host-provisioned blackhole
//! routes. Connection rate limiting and concurrent connection counting from
//! `BackendSecurityConfig` are still not enforced on macOS.
//!
//! **v2 (future)**: Inject iptables rules via `exec()` after boot, or use
//! macOS `pf` rules per VM.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::{debug, error, info, warn};

use crate::backend::control_channel::{ControlChannel, GuestConnector, GUEST_AGENT_PORT};
use crate::backend::{BackendConfig, GuestConsoleSink, VmmBackend};
use crate::error::Result;
use crate::guest::protocol::{
    build_exec_request, ExecOutputChunk, ExecResponse, TelemetrySubscribeRequest,
};
use crate::observe::telemetry::{TelemetryAggregator, TelemetryBuffer};
use crate::observe::tracer::SpanContext;
use crate::observe::Observer;
use crate::ExecOutput;

use super::config;
use super::snapshot::VzSnapshotMeta;
use super::vsock::VzSocketStream;

// ObjC imports for Virtualization.framework
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSFileHandle, NSString, NSURL};
use objc2_virtualization::*;

const GUEST_NETWORK_DENY_LIST_PATH: &str = "/etc/voidbox/network_deny_list.json";

#[async_trait::async_trait]
trait GuestPolicyWriter: Sync {
    async fn mkdir_p(&self, path: &str) -> Result<()>;
    async fn write_file(&self, path: &str, content: &[u8]) -> Result<()>;
}

async fn provision_network_deny_list_with_writer(
    writer: &dyn GuestPolicyWriter,
    deny_list: &[String],
) -> Result<()> {
    if deny_list.is_empty() {
        return Ok(());
    }

    let deny_list_json = serde_json::to_string_pretty(deny_list).map_err(|e| {
        crate::Error::Config(format!("failed to serialize network deny list: {}", e))
    })?;
    writer.mkdir_p("/etc/voidbox").await?;
    writer
        .write_file(GUEST_NETWORK_DENY_LIST_PATH, deny_list_json.as_bytes())
        .await
}

fn deterministic_mac_address(session_secret: &[u8; 32]) -> Retained<VZMACAddress> {
    let mut octets = [0u8; 6];
    octets.copy_from_slice(&session_secret[..6]);
    // Locally administered, unicast MAC address.
    octets[0] = (octets[0] | 0x02) & 0xfe;
    let mac_string = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        octets[0], octets[1], octets[2], octets[3], octets[4], octets[5]
    );
    unsafe {
        VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(&mac_string))
            .expect("deterministic MAC address must be valid")
    }
}

/// Wrapper to assert `Send + Sync` for `Retained<VZVirtioSocketDevice>`.
///
/// # Safety
///
/// The only operation performed on the device from another thread is
/// `connectToPort:completionHandler:`, which dispatches work to the VZ
/// internal queue and is documented as safe to call from any thread.
struct SendSyncDevice(Retained<VZVirtioSocketDevice>);
unsafe impl Send for SendSyncDevice {}
unsafe impl Sync for SendSyncDevice {}

/// macOS Virtualization.framework backend.
///
/// Wraps a `VZVirtualMachine` and communicates with the guest agent
/// via a `ControlChannel` over virtio-socket.
pub struct VzBackend {
    /// The running VZ virtual machine (set after `start()`).
    vm: Option<Retained<VZVirtualMachine>>,
    /// The virtio socket device (needed to connect to the guest).
    socket_device: Option<SendSyncDevice>,
    /// Transport-agnostic control channel for guest communication.
    control_channel: Option<Arc<ControlChannel>>,
    /// Dedicated serial dispatch queue for VZ operations.
    vz_queue: DispatchRetained<DispatchQueue>,
    /// Whether the VM is currently running.
    running: Arc<AtomicBool>,
    /// The assigned CID.
    cid: u32,
    /// Active span context for TRACEPARENT propagation.
    span_context: Option<SpanContext>,
    /// Session secret (kept for snapshot sidecar).
    session_secret: Option<[u8; 32]>,
    /// Snapshot of the BackendConfig used in start() (kept for snapshot sidecar).
    started_config: Option<StartedConfigInfo>,
    /// Full backend config for restart flows such as auto-snapshot.
    start_config: Option<BackendConfig>,
}

/// Subset of BackendConfig we need to persist in the snapshot sidecar.
struct StartedConfigInfo {
    memory_mb: usize,
    vcpus: usize,
    network: bool,
    boot_clock_secs: u64,
}

// Safety: The ObjC `vm` and `socket_device` handles are only mutated in
// `start()` (set) and `stop()` (clear), both of which take `&mut self`
// guaranteeing exclusive access. All guest communication goes through
// `Arc<ControlChannel>` which is already Send + Sync.
unsafe impl Send for VzBackend {}
unsafe impl Sync for VzBackend {}

fn guest_console_sink(config: &BackendConfig) -> Option<Retained<NSFileHandle>> {
    match &config.guest_console {
        GuestConsoleSink::Disabled => {
            debug!("VzBackend: routing guest serial console to null device");
            Some(NSFileHandle::fileHandleWithNullDevice())
        }
        GuestConsoleSink::Stderr => {
            debug!("VzBackend: routing guest serial console to stderr");
            Some(NSFileHandle::fileHandleWithStandardError())
        }
        GuestConsoleSink::File(path) => {
            if let Some(parent) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    warn!(
                        "VzBackend: failed to create guest console log dir {}: {}",
                        parent.display(),
                        e
                    );
                }
            }

            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => {
                    debug!(
                        "VzBackend: routing guest serial console to {}",
                        path.display()
                    );
                    let fd = std::os::fd::IntoRawFd::into_raw_fd(file);
                    Some(NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                        NSFileHandle::alloc(),
                        fd,
                        true,
                    ))
                }
                Err(e) => {
                    warn!(
                        "VzBackend: failed to open guest console log {}: {}; falling back to null device",
                        path.display(),
                        e
                    );
                    Some(NSFileHandle::fileHandleWithNullDevice())
                }
            }
        }
    }
}

impl Default for VzBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VzBackend {
    async fn provision_network_deny_list(&self, deny_list: &[String]) -> Result<()> {
        provision_network_deny_list_with_writer(self, deny_list).await
    }
}

#[async_trait::async_trait]
impl GuestPolicyWriter for VzBackend {
    async fn mkdir_p(&self, path: &str) -> Result<()> {
        VmmBackend::mkdir_p(self, path).await
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        VmmBackend::write_file(self, path, content).await
    }
}

/// Format a Virtualization.framework [`NSError`] with domain, code, and any
/// extra keys Apple populates (`localizedFailureReason`, etc.). The generic
/// `localizedDescription` alone is often useless (e.g. "Internal Virtualization error").
fn format_vz_ns_error(err: *mut objc2_foundation::NSError) -> String {
    if err.is_null() {
        return "(null NSError)".to_string();
    }
    let e = unsafe { &*err };
    let domain = e.domain().to_string();
    let code = e.code();
    let desc = e.localizedDescription().to_string();
    let mut out = format!("{desc} (domain={domain}, code={code})");
    if let Some(reason) = e.localizedFailureReason() {
        out.push_str(&format!("; failure_reason={reason}"));
    }
    if let Some(sugg) = e.localizedRecoverySuggestion() {
        out.push_str(&format!("; recovery_suggestion={sugg}"));
    }
    out
}

/// Dispatch a VZ completion-handler operation and wait for the result.
///
/// Shared helper for pause/resume/save/restore — all follow the same
/// pattern: dispatch onto vz_queue, call an ObjC method that takes a
/// `completionHandler:(NSError*)`, and channel the result back.
fn dispatch_vz_op<F>(
    vz_queue: &DispatchRetained<DispatchQueue>,
    vm_ptr: usize,
    timeout_secs: u64,
    op_name: &str,
    f: F,
) -> Result<()>
where
    F: FnOnce(&VZVirtualMachine, &RcBlock<dyn Fn(*mut objc2_foundation::NSError)>) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
    let op = op_name.to_string();

    vz_queue.exec_async(move || {
        let vm_ref = unsafe { &*(vm_ptr as *const VZVirtualMachine) };
        let tx = std::sync::Mutex::new(Some(tx));
        let op_clone = op.clone();
        let handler = RcBlock::new(move |err: *mut objc2_foundation::NSError| {
            let result = if err.is_null() {
                Ok(())
            } else {
                Err(format_vz_ns_error(err))
            };
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(result);
            }
        });
        f(vm_ref, &handler);
        let _ = op_clone; // prevent the string from being dropped too early
    });

    rx.recv_timeout(std::time::Duration::from_secs(timeout_secs))
        .map_err(|_| crate::Error::Backend(format!("VZ {op_name}: timed out ({timeout_secs}s)")))?
        .map_err(|e| crate::Error::Backend(format!("VZ {op_name} failed: {e}")))
}

impl VzBackend {
    /// Create a new, unstarted VzBackend.
    pub fn new() -> Self {
        let vz_queue = DispatchQueue::new("com.voidbox.vz", DispatchQueueAttr::SERIAL);
        Self {
            vm: None,
            socket_device: None,
            control_channel: None,
            vz_queue,
            running: Arc::new(AtomicBool::new(false)),
            cid: 3, // default; overridden in start()
            span_context: None,
            session_secret: None,
            started_config: None,
            start_config: None,
        }
    }

    /// Build a `VZVirtualMachineConfiguration` from a `BackendConfig`.
    ///
    /// This contains steps 1–7 of the original `start()`: boot loader,
    /// memory/cpu, vsock, networking, serial, virtiofs, and validation.
    /// Both cold-boot and snapshot-restore paths call this.
    fn configure_vm(
        config: &BackendConfig,
        boot_clock_secs: u64,
    ) -> Result<Retained<VZVirtualMachineConfiguration>> {
        // 1. Boot loader
        let kernel_url =
            NSURL::fileURLWithPath(&NSString::from_str(config.kernel.to_str().unwrap_or("")));
        let boot_loader = unsafe {
            VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel_url)
        };

        // Set initramfs
        if let Some(ref initrd) = config.initramfs {
            let initrd_url =
                NSURL::fileURLWithPath(&NSString::from_str(initrd.to_str().unwrap_or("")));
            unsafe { boot_loader.setInitialRamdiskURL(Some(&initrd_url)) };
        }

        // Set kernel cmdline
        let cmdline = config::build_kernel_cmdline_with_clock(config, boot_clock_secs);
        unsafe {
            boot_loader.setCommandLine(&NSString::from_str(&cmdline));
        }
        debug!("VzBackend: kernel cmdline = {}", cmdline);

        // 2. VM configuration
        let vm_config = unsafe { VZVirtualMachineConfiguration::new() };
        unsafe {
            vm_config.setBootLoader(Some(&boot_loader));
            vm_config.setMemorySize(config::memory_bytes(config));
            vm_config.setCPUCount(config.vcpus);
        }

        // 3. Virtio socket device (for host↔guest control channel)
        let vsock_config = unsafe { VZVirtioSocketDeviceConfiguration::new() };
        let socket_configs: Retained<NSArray<VZSocketDeviceConfiguration>> =
            NSArray::arrayWithObject(&vsock_config);
        unsafe {
            vm_config.setSocketDevices(&socket_configs);
        }

        // 4. NAT networking (if enabled)
        if config.network {
            let nat_attachment = unsafe { VZNATNetworkDeviceAttachment::new() };
            let net_config = unsafe { VZVirtioNetworkDeviceConfiguration::new() };
            let mac_address = deterministic_mac_address(&config.security.session_secret);
            unsafe {
                net_config.setAttachment(Some(&nat_attachment));
                net_config.setMACAddress(&mac_address);
            }
            let net_configs: Retained<NSArray<VZNetworkDeviceConfiguration>> =
                NSArray::arrayWithObject(&net_config);
            unsafe {
                vm_config.setNetworkDevices(&net_configs);
            }
        }

        // 5. Serial console for guest kernel/init output.
        let console_sink = guest_console_sink(config)
            .expect("guest console sink must always provide a VZ serial attachment");
        let serial_config = unsafe { VZVirtioConsoleDeviceSerialPortConfiguration::new() };
        let stdio_attachment = unsafe {
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,
                Some(&console_sink),
            )
        };
        unsafe {
            serial_config.setAttachment(Some(&stdio_attachment));
        }
        let serial_configs: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::arrayWithObject(&serial_config);
        unsafe {
            vm_config.setSerialPorts(&serial_configs);
        }

        // 6. Shared directories (virtiofs)
        {
            let mut fs_configs: Vec<Retained<VZVirtioFileSystemDeviceConfiguration>> = Vec::new();

            // Legacy single shared_dir
            if let Some(ref shared_dir) = config.shared_dir {
                if let Some(path_str) = shared_dir.to_str() {
                    let tag = NSString::from_str("shared");
                    let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
                    let share = unsafe {
                        VZSharedDirectory::initWithURL_readOnly(
                            VZSharedDirectory::alloc(),
                            &url,
                            false,
                        )
                    };
                    let single = unsafe {
                        VZSingleDirectoryShare::initWithDirectory(
                            VZSingleDirectoryShare::alloc(),
                            &share,
                        )
                    };
                    let fs = unsafe {
                        VZVirtioFileSystemDeviceConfiguration::initWithTag(
                            VZVirtioFileSystemDeviceConfiguration::alloc(),
                            &tag,
                        )
                    };
                    unsafe { fs.setShare(Some(&single)) };
                    debug!("VzBackend: virtiofs share 'shared' -> {}", path_str);
                    fs_configs.push(fs);
                }
            }

            // Named mounts from config.mounts
            for (i, mount) in config.mounts.iter().enumerate() {
                let tag_str = format!("mount{}", i);
                let tag = NSString::from_str(&tag_str);
                let url = NSURL::fileURLWithPath(&NSString::from_str(&mount.host_path));
                let share = unsafe {
                    VZSharedDirectory::initWithURL_readOnly(
                        VZSharedDirectory::alloc(),
                        &url,
                        mount.read_only,
                    )
                };
                let single = unsafe {
                    VZSingleDirectoryShare::initWithDirectory(
                        VZSingleDirectoryShare::alloc(),
                        &share,
                    )
                };
                let fs = unsafe {
                    VZVirtioFileSystemDeviceConfiguration::initWithTag(
                        VZVirtioFileSystemDeviceConfiguration::alloc(),
                        &tag,
                    )
                };
                unsafe { fs.setShare(Some(&single)) };
                debug!(
                    "VzBackend: virtiofs share '{}' -> {} (ro={})",
                    tag_str, mount.host_path, mount.read_only
                );
                fs_configs.push(fs);
            }

            if !fs_configs.is_empty() {
                // Build NSArray of VZDirectorySharingDeviceConfiguration
                let configs_refs: Vec<&VZDirectorySharingDeviceConfiguration> = fs_configs
                    .iter()
                    .map(|c| {
                        // Safety: VZVirtioFileSystemDeviceConfiguration is a subclass of
                        // VZDirectorySharingDeviceConfiguration
                        let ptr: *const VZVirtioFileSystemDeviceConfiguration = &**c;
                        unsafe { &*(ptr as *const VZDirectorySharingDeviceConfiguration) }
                    })
                    .collect();
                let arr = NSArray::from_slice(&configs_refs);
                unsafe { vm_config.setDirectorySharingDevices(&arr) };
            }
        }

        // 7. Validate configuration
        unsafe {
            vm_config
                .validateWithError()
                .map_err(|e| crate::Error::Backend(format!("VZ config validation: {}", e)))?;
        }

        Ok(vm_config)
    }

    /// Extract socket device from a running VM and set up the control channel.
    fn setup_control_channel(&mut self, session_secret: [u8; 32]) {
        let vm_ref = self.vm.as_ref().unwrap();
        let socket_devices = unsafe { vm_ref.socketDevices() };
        let socket_device = socket_devices.objectAtIndex(0);
        let socket_device: Retained<VZVirtioSocketDevice> =
            unsafe { Retained::cast_unchecked(socket_device) };
        let socket_device = SendSyncDevice(socket_device);

        let connector = Self::build_connector(&socket_device, &self.vz_queue);
        let control_channel = Arc::new(ControlChannel::new(connector, session_secret));

        self.socket_device = Some(socket_device);
        self.control_channel = Some(control_channel);
        self.session_secret = Some(session_secret);
        self.running.store(true, Ordering::SeqCst);
    }

    /// Connect to the guest agent via the VZ virtio socket device.
    ///
    /// Uses `VZVirtioSocketDevice.connectToPort:completionHandler:` which
    /// calls the completion handler with a `VZVirtioSocketConnection`.
    /// The connection's `fileDescriptor()` gives us a raw fd for I/O.
    fn build_connector(
        socket_device: &SendSyncDevice,
        vz_queue: &DispatchRetained<DispatchQueue>,
    ) -> GuestConnector {
        let device = Arc::new(SendSyncDevice(socket_device.0.clone()));
        let queue = vz_queue.clone();
        Arc::new(move || {
            let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<i32, String>>();

            // Dispatch connectToPort onto the VZ queue (required by Apple).
            // Clone the Arc so the device stays alive even if the outer
            // scope is dropped on timeout.
            let device_clone = Arc::clone(&device);
            let tx_clone = tx;
            queue.exec_async(move || {
                let device = &*device_clone;
                let tx = tx_clone;
                let handler = RcBlock::new(
                    move |connection: *mut VZVirtioSocketConnection,
                          err: *mut objc2_foundation::NSError| {
                        if !err.is_null() {
                            let desc = format_vz_ns_error(err);
                            debug!("VZ vsock connectToPort: error = {}", desc);
                            let _ = tx.send(Err(desc));
                            return;
                        }
                        if connection.is_null() {
                            debug!("VZ vsock connectToPort: null connection");
                            let _ = tx.send(Err("null connection".into()));
                            return;
                        }
                        let raw_fd = unsafe { (*connection).fileDescriptor() };
                        let duped_fd = unsafe { libc::dup(raw_fd) };
                        if duped_fd < 0 {
                            let _ = tx.send(Err(format!(
                                "dup fd failed: {}",
                                std::io::Error::last_os_error()
                            )));
                            return;
                        }
                        debug!("VZ vsock connectToPort: success (fd={})", duped_fd);
                        let _ = tx.send(Ok(duped_fd));
                    },
                );
                unsafe {
                    device
                        .0
                        .connectToPort_completionHandler(GUEST_AGENT_PORT, &handler);
                }
            });

            let fd = rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .map_err(|_| crate::Error::Backend("VZ vsock connect timeout".into()))?
                .map_err(|e| crate::Error::Backend(format!("VZ vsock connect: {}", e)))?;

            let stream = VzSocketStream::from_fd(fd);
            Ok(Box::new(stream) as Box<dyn crate::backend::control_channel::GuestStream>)
        })
    }

    /// Pause the running VM.
    pub fn pause(&self) -> Result<()> {
        let vm = self
            .vm
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?;
        let vm_ptr = Retained::as_ptr(vm) as usize;
        info!("VzBackend: pausing VM");
        dispatch_vz_op(&self.vz_queue, vm_ptr, 30, "pause", |vm_ref, handler| {
            unsafe { vm_ref.pauseWithCompletionHandler(handler) };
        })?;
        info!("VzBackend: VM paused");
        Ok(())
    }

    /// Resume a paused VM.
    pub fn resume(&self) -> Result<()> {
        let vm = self
            .vm
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?;
        let vm_ptr = Retained::as_ptr(vm) as usize;
        info!("VzBackend: resuming VM");
        dispatch_vz_op(&self.vz_queue, vm_ptr, 30, "resume", |vm_ref, handler| {
            unsafe { vm_ref.resumeWithCompletionHandler(handler) };
        })?;
        info!("VzBackend: VM resumed");
        Ok(())
    }

    /// Create a snapshot of the running VM.
    ///
    /// Pauses the VM, saves state to Apple's opaque file + our JSON sidecar,
    /// then resumes the VM. The snapshot directory will contain:
    /// - `vm.vzvmsave` — Apple's opaque save file
    /// - `vz_meta.json` — our sidecar metadata
    pub fn create_snapshot(&self, dir: &Path) -> Result<()> {
        let vm = self
            .vm
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?;

        let config_info = self
            .started_config
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("no config info (VM not started?)".into()))?;

        let session_secret = self
            .session_secret
            .ok_or_else(|| crate::Error::Backend("no session secret".into()))?;

        std::fs::create_dir_all(dir)
            .map_err(|e| crate::Error::Snapshot(format!("create snapshot dir: {e}")))?;

        // 1. Pause
        self.pause()?;

        // 2. Save VM state to Apple's opaque file
        let save_path = VzSnapshotMeta::save_file_path(dir);
        let save_url_str = save_path
            .to_str()
            .ok_or_else(|| crate::Error::Snapshot("snapshot path is not valid UTF-8".into()))?;

        let vm_ptr = Retained::as_ptr(vm) as usize;
        let url_string = save_url_str.to_string();

        info!("VzBackend: saving VM state to {}", save_url_str);

        let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
        self.vz_queue.exec_async(move || {
            let vm_ref = unsafe { &*(vm_ptr as *const VZVirtualMachine) };
            let url = NSURL::fileURLWithPath(&NSString::from_str(&url_string));
            let tx = std::sync::Mutex::new(Some(tx));
            let handler = RcBlock::new(move |err: *mut objc2_foundation::NSError| {
                let result = if err.is_null() {
                    Ok(())
                } else {
                    Err(format_vz_ns_error(err))
                };
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(result);
                }
            });
            unsafe {
                vm_ref.saveMachineStateToURL_completionHandler(&url, &handler);
            }
        });

        let save_result = rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .map_err(|_| crate::Error::Snapshot("VM save: timed out (120s)".into()))
            .and_then(|r| r.map_err(|e| crate::Error::Snapshot(format!("VM save failed: {e}"))));

        if let Err(e) = &save_result {
            error!("VzBackend: save failed, resuming VM: {}", e);
            let _ = self.resume();
            return Err(save_result.unwrap_err());
        }

        info!("VzBackend: VM state saved");

        // 3. Save our sidecar metadata
        let meta = VzSnapshotMeta {
            session_secret: session_secret.to_vec(),
            memory_mb: config_info.memory_mb,
            vcpus: config_info.vcpus,
            network: config_info.network,
            cid: self.cid,
            boot_clock_secs: config_info.boot_clock_secs,
        };
        if let Err(e) = meta.save(dir) {
            error!("VzBackend: sidecar save failed, resuming VM: {}", e);
            let _ = self.resume();
            return Err(e);
        }

        // 4. Resume
        self.resume()?;

        info!("VzBackend: snapshot created at {}", dir.display());
        Ok(())
    }

    fn clear_runtime_state(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.control_channel = None;
        self.socket_device = None;
        self.vm = None;
        self.session_secret = None;
        self.started_config = None;
    }
}

#[async_trait::async_trait]
impl VmmBackend for VzBackend {
    async fn start(&mut self, config: BackendConfig) -> Result<()> {
        self.start_config = Some(config.clone());
        if let Some(warning) = config.initramfs_memory_warning() {
            warn!("VzBackend: {}", warning);
        }
        // All ObjC types are !Send, so we run the entire VM setup
        // synchronously via block_in_place to avoid holding them across
        // an .await point.
        tokio::task::block_in_place(|| {
            // ---------------------------------------------------------------
            // Snapshot restore path
            // ---------------------------------------------------------------
            if let Some(ref snapshot_dir) = config.snapshot {
                info!(
                    "VzBackend: restoring VM from snapshot {}",
                    snapshot_dir.display()
                );

                // 1. Load sidecar metadata
                let meta = VzSnapshotMeta::load(snapshot_dir)?;
                let session_secret: [u8; 32] =
                    meta.session_secret.as_slice().try_into().map_err(|_| {
                        crate::Error::Snapshot("invalid session_secret length in snapshot".into())
                    })?;

                // 2. Build VM configuration (same setup as cold boot)
                let vm_config = Self::configure_vm(&config, meta.boot_clock_secs)?;

                // 3. Create VM on the VZ queue
                let vm = unsafe {
                    VZVirtualMachine::initWithConfiguration_queue(
                        VZVirtualMachine::alloc(),
                        &vm_config,
                        &self.vz_queue,
                    )
                };
                self.vm = Some(vm);

                // 4. Restore VM state from Apple's save file
                let save_path = VzSnapshotMeta::save_file_path(snapshot_dir);
                let save_url_str = save_path.to_str().ok_or_else(|| {
                    crate::Error::Snapshot("snapshot path is not valid UTF-8".into())
                })?;
                let url_string = save_url_str.to_string();

                let vm_ptr = Retained::as_ptr(self.vm.as_ref().unwrap()) as usize;

                let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
                let vz_queue = self.vz_queue.clone();
                vz_queue.exec_async(move || {
                    let vm_ref = unsafe { &*(vm_ptr as *const VZVirtualMachine) };
                    let url = NSURL::fileURLWithPath(&NSString::from_str(&url_string));
                    let tx = std::sync::Mutex::new(Some(tx));
                    let handler = RcBlock::new(move |err: *mut objc2_foundation::NSError| {
                        let result = if err.is_null() {
                            Ok(())
                        } else {
                            Err(format_vz_ns_error(err))
                        };
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send(result);
                        }
                    });
                    unsafe {
                        vm_ref.restoreMachineStateFromURL_completionHandler(&url, &handler);
                    }
                });

                if let Err(e) = rx
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .map_err(|_| crate::Error::Backend("VM restore: timed out (60s)".into()))
                    .and_then(|r| {
                        r.map_err(|e| crate::Error::Backend(format!("VM restore failed: {e}")))
                    })
                {
                    self.vm = None;
                    return Err(e);
                }

                info!("VzBackend: VM state restored, resuming");

                // 5. Resume the VM (restore leaves it in Paused state)
                let vm_ptr = Retained::as_ptr(self.vm.as_ref().unwrap()) as usize;
                dispatch_vz_op(
                    &self.vz_queue,
                    vm_ptr,
                    30,
                    "resume (post-restore)",
                    |vm_ref, handler| {
                        unsafe { vm_ref.resumeWithCompletionHandler(handler) };
                    },
                )?;

                info!("VzBackend: VM resumed after restore");

                // 6. Set up socket device + control channel
                self.cid = meta.cid;
                self.started_config = Some(StartedConfigInfo {
                    memory_mb: meta.memory_mb,
                    vcpus: meta.vcpus,
                    network: meta.network,
                    boot_clock_secs: meta.boot_clock_secs,
                });
                self.setup_control_channel(session_secret);

                return Ok(());
            }

            // ---------------------------------------------------------------
            // Cold-boot path
            // ---------------------------------------------------------------
            info!(
                "VzBackend: starting VM (memory={}MB, vcpus={})",
                config.memory_mb, config.vcpus
            );
            if config.network
                && (config.security.max_connections_per_second > 0
                    || config.security.max_concurrent_connections > 0)
            {
                warn!(
                    "VzBackend: macOS VZ NAT does not enforce connection rate/concurrency limits; \
                     deny-list CIDRs are enforced in-guest only"
                );
            }

            let boot_clock_secs = config::current_epoch_secs();
            let vm_config = Self::configure_vm(&config, boot_clock_secs)?;

            // Create and start the VM on a dedicated serial dispatch queue.
            //
            // VZVirtualMachine requires all operations (start, stop,
            // connectToPort) to happen on the queue it was created on.
            // Using a dedicated GCD serial queue means completion handlers
            // fire on GCD-managed threads without needing to pump any run
            // loop — essential for tokio-based CLI apps.
            let vm = unsafe {
                VZVirtualMachine::initWithConfiguration_queue(
                    VZVirtualMachine::alloc(),
                    &vm_config,
                    &self.vz_queue,
                )
            };

            // Store VM in self *before* dispatching the async start so that
            // the Retained keeps the ObjC object alive even if we hit the
            // recv_timeout below. Without this, a timeout would drop the
            // local `vm`, leaving the async closure with a dangling pointer.
            self.vm = Some(vm);

            let vm_ptr = Retained::as_ptr(self.vm.as_ref().unwrap()) as usize;
            dispatch_vz_op(&self.vz_queue, vm_ptr, 30, "start", |vm_ref, handler| {
                unsafe { vm_ref.startWithCompletionHandler(handler) };
            })
            .inspect_err(|_| {
                // Start failed — clear the stored VM so stop() doesn't
                // try to use a half-started machine.
                self.vm = None;
            })?;

            info!("VzBackend: VM started successfully");

            // Store config info for snapshot sidecar
            self.started_config = Some(StartedConfigInfo {
                memory_mb: config.memory_mb,
                vcpus: config.vcpus,
                network: config.network,
                boot_clock_secs,
            });
            self.setup_control_channel(config.security.session_secret);

            Ok(())
        })?;

        self.provision_network_deny_list(&config.security.network_deny_list)
            .await?;

        Ok(())
    }

    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
        env: &[(String, String)],
        working_dir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?;
        let request = build_exec_request(
            program,
            args,
            stdin,
            env,
            working_dir,
            timeout_secs,
            self.span_context.as_ref(),
        );
        let response = cc.send_exec_request(&request).await?;
        Ok(ExecOutput::new(
            response.stdout,
            response.stderr,
            response.exit_code,
        ))
    }

    async fn exec_streaming(
        &self,
        program: &str,
        args: &[&str],
        env: &[(String, String)],
        working_dir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<(
        tokio::sync::mpsc::Receiver<ExecOutputChunk>,
        tokio::sync::oneshot::Receiver<Result<ExecResponse>>,
    )> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?
            .clone();
        let request = build_exec_request(
            program,
            args,
            &[],
            env,
            working_dir,
            timeout_secs,
            self.span_context.as_ref(),
        );

        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(256);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        tokio::task::spawn(async move {
            let result = cc
                .send_exec_request_streaming_async(&request, chunk_tx)
                .await;
            let _ = done_tx.send(result);
        });

        Ok((chunk_rx, done_rx))
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?;

        let resp = cc.send_write_file(path, content).await?;
        if !resp.success {
            return Err(crate::Error::Backend(format!(
                "write_file failed: {}",
                resp.error.unwrap_or_default()
            )));
        }
        Ok(())
    }

    async fn mkdir_p(&self, path: &str) -> Result<()> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?;

        let resp = cc.send_mkdir_p(path).await?;
        if !resp.success {
            return Err(crate::Error::Backend(format!(
                "mkdir_p failed: {}",
                resp.error.unwrap_or_default()
            )));
        }
        Ok(())
    }

    async fn file_stat(&self, path: &str) -> Result<crate::guest::protocol::FileStatResponse> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or(crate::Error::VmNotRunning)?;
        cc.send_file_stat(path).await
    }

    async fn read_file_native(&self, path: &str) -> Result<Vec<u8>> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or(crate::Error::VmNotRunning)?;
        let response = cc.send_read_file(path).await?;
        if response.success {
            Ok(response.content)
        } else {
            Err(crate::Error::Backend(format!(
                "Failed to read file: {}",
                response.error.unwrap_or_default()
            )))
        }
    }

    async fn start_telemetry(
        &mut self,
        observer: Observer,
        opts: TelemetrySubscribeRequest,
        ring_buffer: Option<TelemetryBuffer>,
    ) -> Result<Arc<TelemetryAggregator>> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?
            .clone();

        let aggregator = Arc::new(match ring_buffer {
            Some(rb) => TelemetryAggregator::with_ring_buffer(observer, self.cid, rb),
            None => TelemetryAggregator::new(observer, self.cid),
        });
        let agg_clone = aggregator.clone();

        tokio::spawn(async move {
            if let Err(e) = cc
                .subscribe_telemetry(&opts, move |batch| {
                    agg_clone.ingest(&batch);
                })
                .await
            {
                warn!("Telemetry subscription ended: {}", e);
            }
        });

        Ok(aggregator)
    }

    fn set_span_context(&mut self, ctx: SpanContext) {
        self.span_context = Some(ctx);
    }

    async fn attach_pty(
        &self,
        request: void_box_protocol::PtyOpenRequest,
    ) -> Result<super::super::pty_session::PtySession> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or(crate::Error::VmNotRunning)?;
        cc.open_pty(request).await
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn stop(&mut self) -> Result<()> {
        tokio::task::block_in_place(|| {
            if let Some(ref vm) = self.vm {
                info!("VzBackend: stopping VM");

                let vm_ptr = Retained::as_ptr(vm) as usize;
                dispatch_vz_op(&self.vz_queue, vm_ptr, 10, "stop", |vm_ref, handler| {
                    unsafe { vm_ref.stopWithCompletionHandler(handler) };
                })
                .inspect_err(|e| error!("VzBackend: VM stop error: {}", e))?;
                info!("VzBackend: VM stopped");
            }

            self.clear_runtime_state();
            Ok(())
        })
    }

    async fn create_auto_snapshot(
        &mut self,
        snapshot_dir: &std::path::Path,
        _config_hash: String,
    ) -> Result<()> {
        let channel = self
            .control_channel
            .as_ref()
            .ok_or_else(|| crate::Error::Guest("no control channel".into()))?;
        channel
            .wait_for_snapshot_ready(std::time::Duration::from_secs(30))
            .await?;

        self.create_snapshot(snapshot_dir)?;
        info!(
            "VzBackend: auto-snapshot saved to {}",
            snapshot_dir.display()
        );
        warn!(
            "VzBackend: auto-snapshot restore is currently disabled on macOS; \
             keeping the current VM running after saving the snapshot"
        );
        Ok(())
    }

    fn cid(&self) -> u32 {
        self.cid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendSecurityConfig;
    use crate::Result;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakePolicyWriter {
        mkdir_calls: Mutex<Vec<String>>,
        write_calls: Mutex<Vec<(String, Vec<u8>)>>,
    }

    #[async_trait::async_trait]
    impl GuestPolicyWriter for FakePolicyWriter {
        async fn mkdir_p(&self, path: &str) -> Result<()> {
            self.mkdir_calls.lock().unwrap().push(path.to_string());
            Ok(())
        }

        async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
            self.write_calls
                .lock()
                .unwrap()
                .push((path.to_string(), content.to_vec()));
            Ok(())
        }
    }

    fn test_security_config() -> BackendSecurityConfig {
        BackendSecurityConfig {
            session_secret: [7u8; 32],
            command_allowlist: Vec::new(),
            network_deny_list: Vec::new(),
            max_connections_per_second: 0,
            max_concurrent_connections: 0,
            seccomp: false,
        }
    }

    fn test_config(sink: GuestConsoleSink) -> BackendConfig {
        BackendConfig {
            memory_mb: 512,
            vcpus: 1,
            kernel: "/tmp/kernel".into(),
            initramfs: None,
            rootfs: None,
            network: false,
            enable_vsock: true,
            guest_console: sink,
            shared_dir: None,
            mounts: Vec::new(),
            oci_rootfs: None,
            oci_rootfs_dev: None,
            oci_rootfs_disk: None,
            env: Vec::new(),
            security: test_security_config(),
            snapshot: None,
        }
    }

    #[test]
    fn guest_console_sink_keeps_attachment_for_disabled() {
        assert!(guest_console_sink(&test_config(GuestConsoleSink::Disabled)).is_some());
    }

    #[test]
    fn guest_console_sink_uses_attachment_for_stderr() {
        assert!(guest_console_sink(&test_config(GuestConsoleSink::Stderr)).is_some());
    }

    #[tokio::test]
    async fn provision_network_deny_list_noops_when_empty() {
        let writer = FakePolicyWriter::default();
        provision_network_deny_list_with_writer(&writer, &[])
            .await
            .unwrap();

        assert!(writer.mkdir_calls.lock().unwrap().is_empty());
        assert!(writer.write_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provision_network_deny_list_writes_expected_file() {
        let writer = FakePolicyWriter::default();
        let deny_list = vec!["192.168.64.1/32".to_string(), "203.0.113.0/24".to_string()];
        provision_network_deny_list_with_writer(&writer, &deny_list)
            .await
            .unwrap();

        assert_eq!(
            writer.mkdir_calls.lock().unwrap().as_slice(),
            ["/etc/voidbox"]
        );

        let writes = writer.write_calls.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, GUEST_NETWORK_DENY_LIST_PATH);
        let written_json: Vec<String> = serde_json::from_slice(&writes[0].1).unwrap();
        assert_eq!(written_json, deny_list);
    }
}
