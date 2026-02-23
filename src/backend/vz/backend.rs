//! VzBackend: [`VmmBackend`] implementation using Apple's Virtualization.framework.
//!
//! ## Lifecycle
//!
//! 1. `start()`: Configures and boots a `VZVirtualMachine` with:
//!    - `VZLinuxBootLoader` (kernel, initrd, cmdline)
//!    - `VZVirtioSocketDeviceConfiguration` (for host↔guest control channel)
//!    - `VZNATNetworkDeviceAttachment` (if networking enabled)
//!    - `VZVirtioFileSystemDeviceConfiguration` (if shared_dir provided)
//! 2. `exec()`, `write_file()`, etc.: Delegate to `ControlChannel` over vsock fd
//! 3. `stop()`: Requests VM stop via Virtualization.framework
//!
//! ## Network Security (v1 limitation)
//!
//! VZ provides `VZNATNetworkDeviceAttachment` which gives NAT networking out of
//! the box. However, unlike Linux/KVM where the SLIRP stack enforces CIDR deny
//! lists, rate limiting, and connection counting at the host level, VZ NAT does
//! **not** provide these controls.
//!
//! **v1**: The VM boundary itself is the isolation primitive. Network deny lists
//! from `BackendSecurityConfig` are not enforced on macOS. Guest-side iptables
//! injection is planned for v2 (requires iptables in the guest rootfs).
//!
//! **v2 (future)**: Inject iptables rules via `exec()` after boot, or use
//! macOS `pf` rules per VM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::{debug, error, info};

use crate::backend::control_channel::{ControlChannel, GuestConnector};
use crate::backend::{BackendConfig, VmmBackend};
use crate::error::Result;
use crate::guest::protocol::{ExecOutputChunk, ExecResponse, TelemetrySubscribeRequest};
use crate::observe::telemetry::TelemetryAggregator;
use crate::observe::tracer::SpanContext;
use crate::observe::Observer;
use crate::ExecOutput;

use super::config;
use super::vsock::VzSocketStream;

// ObjC imports for Virtualization.framework
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_virtualization::*;

/// macOS Virtualization.framework backend.
///
/// Wraps a `VZVirtualMachine` and communicates with the guest agent
/// via a `ControlChannel` over virtio-socket.
pub struct VzBackend {
    /// The running VZ virtual machine (set after `start()`).
    vm: Option<Retained<VZVirtualMachine>>,
    /// The virtio socket device (needed to connect to the guest).
    socket_device: Option<Retained<VZVirtioSocketDevice>>,
    /// Transport-agnostic control channel for guest communication.
    control_channel: Option<Arc<ControlChannel>>,
    /// Whether the VM is currently running.
    running: Arc<AtomicBool>,
    /// The assigned CID.
    cid: u32,
    /// Active span context for TRACEPARENT propagation.
    span_context: Option<SpanContext>,
}

impl VzBackend {
    /// Create a new, unstarted VzBackend.
    pub fn new() -> Self {
        Self {
            vm: None,
            socket_device: None,
            control_channel: None,
            running: Arc::new(AtomicBool::new(false)),
            cid: 3, // default; overridden in start()
            span_context: None,
        }
    }

    /// Connect to the guest agent via the VZ virtio socket device.
    ///
    /// Uses `VZVirtioSocketDevice.connectToPort:completionHandler:` which
    /// calls the completion handler with a `VZVirtioSocketConnection`.
    /// The connection's `fileDescriptor()` gives us a raw fd for I/O.
    fn build_connector(socket_device: &Retained<VZVirtioSocketDevice>) -> GuestConnector {
        let device = socket_device.clone();
        Box::new(move || {
            // Bridge the ObjC completion handler to a blocking Rust call.
            let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<i32, String>>();

            let tx_clone = tx.clone();
            let handler = RcBlock::new(
                move |connection: *mut VZVirtioSocketConnection,
                      err: *mut objc2_foundation::NSError| {
                    if !err.is_null() {
                        let desc = unsafe { &*err }.localizedDescription().to_string();
                        let _ = tx_clone.send(Err(desc));
                        return;
                    }
                    if connection.is_null() {
                        let _ = tx_clone.send(Err("null connection".into()));
                        return;
                    }
                    let fd = unsafe { (*connection).fileDescriptor() };
                    let _ = tx_clone.send(Ok(fd as i32));
                },
            );

            unsafe {
                device.connectToPort_completionHandler(1234, &handler);
            }

            // Wait for the completion handler (with timeout)
            let fd = rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .map_err(|_| crate::Error::Backend("VZ vsock connect timeout".into()))?
                .map_err(|e| crate::Error::Backend(format!("VZ vsock connect: {}", e)))?;

            let stream = unsafe { VzSocketStream::from_raw_fd(fd) };
            Ok(Box::new(stream) as Box<dyn crate::backend::control_channel::GuestStream>)
        })
    }
}

#[async_trait::async_trait]
impl VmmBackend for VzBackend {
    async fn start(&mut self, config: BackendConfig) -> Result<()> {
        info!(
            "VzBackend: starting VM (memory={}MB, vcpus={})",
            config.memory_mb, config.vcpus
        );

        // 1. Boot loader
        let kernel_url = unsafe {
            NSURL::fileURLWithPath(&NSString::from_str(config.kernel.to_str().unwrap_or("")))
        };
        let boot_loader = unsafe {
            VZLinuxBootLoader::initWithKernelURL(&VZLinuxBootLoader::alloc(), &kernel_url)
        };

        // Set initramfs
        if let Some(ref initrd) = config.initramfs {
            let initrd_url = unsafe {
                NSURL::fileURLWithPath(&NSString::from_str(initrd.to_str().unwrap_or("")))
            };
            unsafe { boot_loader.setInitialRamdiskURL(Some(&initrd_url)) };
        }

        // Set kernel cmdline
        let cmdline = config::build_kernel_cmdline(&config);
        unsafe {
            boot_loader.setCommandLine(&NSString::from_str(&cmdline));
        }
        debug!("VzBackend: kernel cmdline = {}", cmdline);

        // 2. VM configuration
        let vm_config = unsafe { VZVirtualMachineConfiguration::new() };
        unsafe {
            vm_config.setBootLoader(Some(&boot_loader));
            vm_config.setMemorySize(config::memory_bytes(&config));
            vm_config.setCPUCount(config.vcpus);
        }

        // 3. Virtio socket device (for host↔guest control channel)
        let vsock_config = unsafe { VZVirtioSocketDeviceConfiguration::new() };
        let socket_configs: Retained<NSArray<AnyObject>> =
            unsafe { NSArray::arrayWithObject(&vsock_config) };
        unsafe {
            vm_config.setSocketDevices(&socket_configs);
        }

        // 4. NAT networking (if enabled)
        if config.network {
            let nat_attachment = unsafe { VZNATNetworkDeviceAttachment::new() };
            let net_config = unsafe { VZVirtioNetworkDeviceConfiguration::new() };
            unsafe {
                net_config.setAttachment(Some(&nat_attachment));
            }
            let net_configs: Retained<NSArray<AnyObject>> =
                unsafe { NSArray::arrayWithObject(&net_config) };
            unsafe {
                vm_config.setNetworkDevices(&net_configs);
            }
        }

        // 5. Shared directory (virtiofs) — M6 enhancement
        // TODO: implement VZVirtioFileSystemDeviceConfiguration when shared_dir is set

        // 6. Validate configuration
        unsafe {
            vm_config
                .validateWithError()
                .map_err(|e| crate::Error::Backend(format!("VZ config validation: {}", e)))?;
        }

        // 7. Create and start the VM
        let vm = unsafe {
            VZVirtualMachine::initWithConfiguration(&VZVirtualMachine::alloc(), &vm_config)
        };

        // Start VM with completion handler
        let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
        let tx = std::sync::Mutex::new(Some(tx));

        let handler = RcBlock::new(move |err: *mut objc2_foundation::NSError| {
            let result = if err.is_null() {
                Ok(())
            } else {
                let desc = unsafe { &*err }.localizedDescription().to_string();
                Err(desc)
            };
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(result);
            }
        });

        unsafe {
            vm.startWithCompletionHandler(&handler);
        }

        rx.await
            .map_err(|_| crate::Error::Backend("VM start: channel closed".into()))?
            .map_err(|e| crate::Error::Backend(format!("VM start failed: {}", e)))?;

        info!("VzBackend: VM started successfully");

        // 8. Get the socket device for vsock connections
        let socket_devices = unsafe { vm.socketDevices() };
        let socket_device = unsafe { socket_devices.objectAtIndex(0) };
        let socket_device: Retained<VZVirtioSocketDevice> =
            unsafe { Retained::cast(socket_device) };

        // 9. Build the control channel
        let connector = Self::build_connector(&socket_device);
        let control_channel = Arc::new(ControlChannel::new(
            connector,
            config.security.session_secret,
        ));

        self.vm = Some(vm);
        self.socket_device = Some(socket_device);
        self.control_channel = Some(control_channel);
        self.running.store(true, Ordering::SeqCst);

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

        let request = crate::guest::protocol::ExecRequest {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdin: stdin.to_vec(),
            env: env.to_vec(),
            working_dir: working_dir.map(|s| s.to_string()),
            timeout_secs,
        };

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

        let request = crate::guest::protocol::ExecRequest {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdin: Vec::new(),
            env: env.to_vec(),
            working_dir: working_dir.map(|s| s.to_string()),
            timeout_secs,
        };

        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(256);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let result = rt.block_on(cc.send_exec_request_streaming(&request, |chunk| {
                let _ = chunk_tx.blocking_send(chunk);
            }));
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

    async fn start_telemetry(
        &mut self,
        observer: Observer,
        opts: TelemetrySubscribeRequest,
    ) -> Result<Arc<TelemetryAggregator>> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or_else(|| crate::Error::Backend("VM not started".into()))?
            .clone();

        let aggregator = Arc::new(TelemetryAggregator::new(self.cid, observer));
        let agg_clone = aggregator.clone();

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let _ = rt.block_on(cc.subscribe_telemetry(&opts, move |batch| {
                agg_clone.ingest(batch);
            }));
        });

        Ok(aggregator)
    }

    fn set_span_context(&mut self, ctx: SpanContext) {
        self.span_context = Some(ctx);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(ref vm) = self.vm {
            info!("VzBackend: stopping VM");

            let (tx, rx) = tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
            let tx = std::sync::Mutex::new(Some(tx));

            let handler = RcBlock::new(move |err: *mut objc2_foundation::NSError| {
                let result = if err.is_null() {
                    Ok(())
                } else {
                    let desc = unsafe { &*err }.localizedDescription().to_string();
                    Err(desc)
                };
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(result);
                }
            });

            unsafe {
                vm.stopWithCompletionHandler(&handler);
            }

            match rx.await {
                Ok(Ok(())) => info!("VzBackend: VM stopped"),
                Ok(Err(e)) => error!("VzBackend: VM stop error: {}", e),
                Err(_) => error!("VzBackend: VM stop channel closed"),
            }
        }

        self.running.store(false, Ordering::SeqCst);
        self.control_channel = None;
        self.socket_device = None;
        self.vm = None;
        Ok(())
    }

    fn cid(&self) -> u32 {
        self.cid
    }
}
