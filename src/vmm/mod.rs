//! VMM (Virtual Machine Monitor) core implementation
//!
//! This module contains the core VMM components:
//! - KVM setup and VM creation
//! - Guest memory management
//! - vCPU configuration and execution
//! - Kernel loading and boot parameter setup

pub mod boot;
pub mod config;
pub mod cpu;
pub mod kvm;
pub mod memory;

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::devices::serial::SerialDevice;
use crate::devices::virtio_net::VirtioNetDevice;
use crate::devices::virtio_vsock::VsockDevice;
use crate::devices::virtio_vsock_mmio::VirtioVsockMmio;
use crate::vmm::cpu::MmioDevices;
use crate::guest::protocol::{
    ExecRequest, ExecResponse,
    WriteFileRequest, WriteFileResponse,
    MkdirPRequest, MkdirPResponse,
};
use crate::network::slirp::SlirpStack;
use crate::observe::telemetry::TelemetryAggregator;
use crate::observe::Observer;
use crate::{Error, ExecOutput, Result};

use self::config::VoidBoxConfig;
use self::cpu::VcpuHandle;
use self::kvm::Vm;

/// Main VoidBox instance representing a running micro-VM
pub struct VoidBox {
    /// The underlying KVM VM
    #[allow(dead_code)]
    vm: Arc<Vm>,
    /// vCPU thread handles
    vcpu_handles: Vec<VcpuHandle>,
    /// Flag indicating if VM is running
    running: Arc<AtomicBool>,
    /// Serial output receiver
    serial_output: mpsc::Receiver<u8>,
    /// Context ID for vsock communication
    cid: u32,
    /// Vsock device for guest communication
    #[allow(dead_code)]
    vsock: Option<Arc<VsockDevice>>,
    /// virtio-net device for SLIRP networking
    #[allow(dead_code)]
    virtio_net: Option<Arc<Mutex<VirtioNetDevice>>>,
    /// Channel to send commands to the VM event loop
    command_tx: mpsc::Sender<VmCommand>,
    /// Handle to the VM event loop thread
    event_loop_handle: Option<JoinHandle<()>>,
    /// Handle to the vsock IRQ handler thread (if vsock enabled)
    vsock_irq_handle: Option<JoinHandle<()>>,
    /// Guest telemetry aggregator (if telemetry is active)
    telemetry: Option<Arc<TelemetryAggregator>>,
    /// Active span context for trace propagation into the guest.
    /// When set, `exec_with_env` will inject a `TRACEPARENT` env var.
    active_span_context: Option<crate::observe::tracer::SpanContext>,
}

/// Commands that can be sent to the VM event loop
enum VmCommand {
    /// Execute a command in the guest
    Exec {
        request: ExecRequest,
        response_tx: oneshot::Sender<Result<ExecResponse>>,
    },
    /// Write a file to the guest filesystem (native protocol, no shell)
    WriteFile {
        request: WriteFileRequest,
        response_tx: oneshot::Sender<Result<WriteFileResponse>>,
    },
    /// Create directories in the guest filesystem (mkdir -p)
    MkdirP {
        request: MkdirPRequest,
        response_tx: oneshot::Sender<Result<MkdirPResponse>>,
    },
    /// Start a telemetry subscription
    SubscribeTelemetry {
        aggregator: Arc<TelemetryAggregator>,
    },
    /// Stop the VM
    Stop,
}

impl VoidBox {
    /// Create and start a new micro-VM with the given configuration
    pub async fn new(config: VoidBoxConfig) -> Result<Self> {
        info!("Creating new VoidBox with config: {:?}", config);

        // Validate configuration
        config.validate()?;

        // Create KVM VM
        let vm = Arc::new(Vm::new(config.memory_mb)?);
        debug!("Created KVM VM");

        // Set up serial device for console output
        let (serial_tx, serial_rx) = mpsc::channel(4096);
        let serial = SerialDevice::new(serial_tx);
        debug!("Created serial device");

        // Load kernel and initramfs
        let entry_point = boot::load_kernel(
            &vm,
            &config.kernel,
            config.initramfs.as_deref(),
            &config.kernel_cmdline(),
        )?;
        debug!("Loaded kernel at entry point: {:#x}", entry_point);

        // CID for vsock (must be > 2; 0-2 reserved)
        let cid = config.cid.unwrap_or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u32;
            3 + (seed % 0xFFFF_FFFC)
        });

        // Vsock device for host->guest exec (connect to guest agent)
        let vsock = if config.enable_vsock {
            Some(Arc::new(VsockDevice::new(cid)?))
        } else {
            None
        };

        // Virtio-vsock MMIO device so the guest has a vsock device (required for host connect to work)
        let virtio_vsock_mmio = if config.enable_vsock {
            match VirtioVsockMmio::new_with_require_vhost(cid, true) {
                Ok(mut dev) => {
                    dev.set_mmio_base(0xd080_0000);
                    info!("virtio-vsock MMIO at {:#x}, CID {}", dev.mmio_base(), cid);
                    Some(Arc::new(Mutex::new(dev)))
                }
                Err(e) => {
                    debug!("virtio-vsock MMIO unavailable: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Virtio-net with SLIRP backend if networking is enabled
        let virtio_net = if config.network {
            debug!("Setting up SLIRP networking");
            let slirp = Arc::new(Mutex::new(SlirpStack::new()?));
            let mut net_device = VirtioNetDevice::new(slirp)?;
            net_device.set_mmio_base(0xd000_0000);
            info!(
                "virtio-net enabled at MMIO {:#x}, MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                net_device.mmio_base(),
                net_device.mac()[0],
                net_device.mac()[1],
                net_device.mac()[2],
                net_device.mac()[3],
                net_device.mac()[4],
                net_device.mac()[5]
            );
            Some(Arc::new(Mutex::new(net_device)))
        } else {
            None
        };

        let mmio_devices = MmioDevices {
            virtio_net,
            virtio_vsock: virtio_vsock_mmio,
        };

        // Create vCPUs (with MMIO dispatch to virtio-net and virtio-vsock)
        let running = Arc::new(AtomicBool::new(true));
        let mut vcpu_handles = Vec::with_capacity(config.vcpus);
        for vcpu_id in 0..config.vcpus {
            let handle = cpu::create_vcpu(
                vm.clone(),
                vcpu_id as u64,
                entry_point,
                running.clone(),
                serial.clone(),
                MmioDevices {
                    virtio_net: mmio_devices.virtio_net.clone(),
                    virtio_vsock: mmio_devices.virtio_vsock.clone(),
                },
            )?;
            vcpu_handles.push(handle);
        }
        debug!("Created {} vCPUs", config.vcpus);

        // Spawn a background thread to handle vhost-vsock interrupts.
        // When the vhost backend writes to a call eventfd, we must:
        //   1. Set INTERRUPT_STATUS |= 1 on the virtio-mmio device (so the guest ISR sees it)
        //   2. Inject IRQ 11 into the guest via the in-kernel irqchip
        // KVM_IRQFD alone is NOT sufficient because virtio-mmio's ISR checks INTERRUPT_STATUS.
        let vsock_irq_handle = if let Some(ref vsock_mmio) = mmio_devices.virtio_vsock {
            let call_fds: Vec<RawFd> = {
                let guard = vsock_mmio.lock().unwrap();
                guard.call_eventfds().iter().filter_map(|f| *f).collect()
            };
            if !call_fds.is_empty() {
                let vsock_mmio_clone = vsock_mmio.clone();
                let vm_fd_raw = vm.vm_fd().as_raw_fd();
                let running_irq = running.clone();
                let handle = std::thread::Builder::new()
                    .name("vsock-irq".into())
                    .spawn(move || {
                        vsock_irq_thread(call_fds, vsock_mmio_clone, vm_fd_raw, running_irq);
                    })
                    .expect("Failed to spawn vsock-irq thread");
                debug!("Spawned vsock-irq handler thread");
                Some(handle)
            } else {
                None
            }
        } else {
            None
        };

        // Create command channel
        let (command_tx, mut command_rx) = mpsc::channel::<VmCommand>(32);

        // Start VM event loop
        let running_clone = running.clone();
        let vsock_clone = vsock.clone();
        let event_loop_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            rt.block_on(async {
                while running_clone.load(Ordering::SeqCst) {
                    tokio::select! {
                        Some(cmd) = command_rx.recv() => {
                            match cmd {
                                VmCommand::Exec { request, response_tx } => {
                                    let result = if let Some(ref vsock) = vsock_clone {
                                        vsock.send_exec_request(&request).await
                                    } else {
                                        Err(Error::Guest("vsock not enabled".into()))
                                    };
                                    let _ = response_tx.send(result);
                                }
                                VmCommand::WriteFile { request, response_tx } => {
                                    let result = if let Some(ref vsock) = vsock_clone {
                                        vsock.send_write_file(&request.path, &request.content).await
                                    } else {
                                        Err(Error::Guest("vsock not enabled".into()))
                                    };
                                    let _ = response_tx.send(result);
                                }
                                VmCommand::MkdirP { request, response_tx } => {
                                    let result = if let Some(ref vsock) = vsock_clone {
                                        vsock.send_mkdir_p(&request.path).await
                                    } else {
                                        Err(Error::Guest("vsock not enabled".into()))
                                    };
                                    let _ = response_tx.send(result);
                                }
                                VmCommand::SubscribeTelemetry { aggregator } => {
                                    if let Some(ref vsock) = vsock_clone {
                                        let vsock = vsock.clone();
                                        let agg = aggregator.clone();
                                        tokio::spawn(async move {
                                            if let Err(e) = vsock.subscribe_telemetry(move |batch| {
                                                agg.ingest(&batch);
                                            }).await {
                                                tracing::warn!("Telemetry subscription ended: {}", e);
                                            }
                                        });
                                    }
                                }
                                VmCommand::Stop => {
                                    running_clone.store(false, Ordering::SeqCst);
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                            // Periodic tick for housekeeping
                        }
                    }
                }
            });
        });

        info!("VoidBox started with CID {}, network={}", cid, config.network);

        Ok(Self {
            vm,
            vcpu_handles,
            running,
            serial_output: serial_rx,
            cid,
            vsock,
            virtio_net: mmio_devices.virtio_net,
            command_tx,
            event_loop_handle: Some(event_loop_handle),
            vsock_irq_handle,
            telemetry: None,
            active_span_context: None,
        })
    }

    /// Execute a command in the guest VM
    pub async fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput> {
        self.exec_with_stdin(program, args, &[]).await
    }

    /// Execute a command in the guest VM with stdin input
    pub async fn exec_with_stdin(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
    ) -> Result<ExecOutput> {
        self.exec_with_env(program, args, stdin, &[], None).await
    }

    /// Execute a command in the guest VM with stdin, environment, and working directory.
    /// Use this to pass e.g. ANTHROPIC_API_KEY or project-specific env into the guest.
    ///
    /// When the `opentelemetry` feature is enabled and there is an active trace context,
    /// a `TRACEPARENT` environment variable is automatically injected so that the guest
    /// process can participate in the distributed trace.
    pub async fn exec_with_env(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
        env: &[(String, String)],
        working_dir: Option<&str>,
    ) -> Result<ExecOutput> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(Error::VmNotRunning);
        }

        // Build env with optional TRACEPARENT injection
        let mut exec_env = env.to_vec();
        if let Some(ref ctx) = self.active_span_context {
            // Only inject if not already present
            if !exec_env.iter().any(|(k, _)| k == "TRACEPARENT") {
                exec_env.push(("TRACEPARENT".to_string(), ctx.to_traceparent()));
            }
        }

        let request = ExecRequest {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdin: stdin.to_vec(),
            env: exec_env,
            working_dir: working_dir.map(String::from),
            timeout_secs: None,
        };

        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(VmCommand::Exec {
                request,
                response_tx,
            })
            .await
            .map_err(|_| Error::Guest("Failed to send command".into()))?;

        let response = response_rx
            .await
            .map_err(|_| Error::Guest("Failed to receive response".into()))??;

        Ok(ExecOutput::new(
            response.stdout,
            response.stderr,
            response.exit_code,
        ))
    }

    /// Write a file to the guest filesystem using the native WriteFile protocol.
    ///
    /// This bypasses shell and base64 encoding -- the guest-agent writes the
    /// file directly in Rust. Parent directories are created automatically.
    pub async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(Error::VmNotRunning);
        }

        let request = WriteFileRequest {
            path: path.to_string(),
            content: content.to_vec(),
            create_parents: true,
        };

        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(VmCommand::WriteFile {
                request,
                response_tx,
            })
            .await
            .map_err(|_| Error::Guest("Failed to send WriteFile command".into()))?;

        let response = response_rx
            .await
            .map_err(|_| Error::Guest("Failed to receive WriteFile response".into()))??;

        if response.success {
            Ok(())
        } else {
            Err(Error::Guest(format!(
                "Failed to write file: {}",
                response.error.unwrap_or_default()
            )))
        }
    }

    /// Create directories in the guest filesystem (mkdir -p).
    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(Error::VmNotRunning);
        }

        let request = MkdirPRequest {
            path: path.to_string(),
        };

        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(VmCommand::MkdirP {
                request,
                response_tx,
            })
            .await
            .map_err(|_| Error::Guest("Failed to send MkdirP command".into()))?;

        let response = response_rx
            .await
            .map_err(|_| Error::Guest("Failed to receive MkdirP response".into()))??;

        if response.success {
            Ok(())
        } else {
            Err(Error::Guest(format!(
                "Failed to create directory: {}",
                response.error.unwrap_or_default()
            )))
        }
    }

    /// Start a telemetry subscription from the guest.
    ///
    /// Creates a `TelemetryAggregator` that feeds guest metrics into the
    /// provided `Observer`. The subscription runs in the background until
    /// the VM stops or the guest connection drops.
    pub async fn start_telemetry(&mut self, observer: Observer) -> Result<Arc<TelemetryAggregator>> {
        let aggregator = Arc::new(TelemetryAggregator::new(observer, self.cid));
        self.telemetry = Some(aggregator.clone());

        self.command_tx
            .send(VmCommand::SubscribeTelemetry {
                aggregator: aggregator.clone(),
            })
            .await
            .map_err(|_| Error::Guest("Failed to send telemetry subscribe command".into()))?;

        info!("Telemetry subscription requested for CID {}", self.cid);
        Ok(aggregator)
    }

    /// Get the telemetry aggregator, if telemetry has been started.
    pub fn telemetry(&self) -> Option<&Arc<TelemetryAggregator>> {
        self.telemetry.as_ref()
    }

    /// Set the active span context for TRACEPARENT propagation.
    ///
    /// Any subsequent `exec_with_env` calls will inject this context as a
    /// `TRACEPARENT` environment variable so guest processes participate
    /// in the distributed trace.
    pub fn set_span_context(&mut self, ctx: crate::observe::tracer::SpanContext) {
        self.active_span_context = Some(ctx);
    }

    /// Clear the active span context.
    pub fn clear_span_context(&mut self) {
        self.active_span_context = None;
    }

    /// Get the vsock CID for this VM
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// Check if the VM is currently running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Read available serial output
    pub fn read_serial_output(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        while let Ok(byte) = self.serial_output.try_recv() {
            output.push(byte);
        }
        output
    }

    /// Stop the VM
    pub async fn stop(&mut self) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        info!("Stopping VoidBox");

        // Signal stop through command channel
        let _ = self.command_tx.send(VmCommand::Stop).await;

        // Signal vCPUs to stop
        self.running.store(false, Ordering::SeqCst);

        // Wait for vCPU threads to finish
        for handle in self.vcpu_handles.drain(..) {
            handle.join()?;
        }

        // Wait for event loop to finish
        if let Some(handle) = self.event_loop_handle.take() {
            handle.join().map_err(|_| Error::Vcpu("Event loop panic".into()))?;
        }

        // Wait for vsock IRQ handler if present
        if let Some(handle) = self.vsock_irq_handle.take() {
            handle.join().map_err(|_| Error::Vcpu("vsock-irq thread panic".into()))?;
        }

        info!("VoidBox stopped");
        Ok(())
    }
}

impl Drop for VoidBox {
    fn drop(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            self.running.store(false, Ordering::SeqCst);
            error!("VoidBox dropped while still running - forcing stop");
        }
    }
}

/// Background thread that bridges vhost-vsock call eventfds to virtio-mmio interrupts.
///
/// When the vhost backend has data for the guest, it writes to a call eventfd.
/// This thread detects that signal, sets the MMIO device's INTERRUPT_STATUS register,
/// and injects IRQ 11 into the guest via the in-kernel irqchip (KVM_IRQ_LINE).
fn vsock_irq_thread(
    call_fds: Vec<RawFd>,
    vsock_mmio: Arc<Mutex<VirtioVsockMmio>>,
    vm_fd: RawFd,
    running: Arc<AtomicBool>,
) {
    use libc::{epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLLIN};

    let epfd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
    if epfd < 0 {
        error!("vsock-irq: epoll_create1 failed: {}", std::io::Error::last_os_error());
        return;
    }

    for (i, &fd) in call_fds.iter().enumerate() {
        let mut ev = epoll_event { events: EPOLLIN as u32, u64: i as u64 };
        let ret = unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev) };
        if ret < 0 {
            error!("vsock-irq: epoll_ctl ADD fd={} failed: {}", fd, std::io::Error::last_os_error());
        }
    }

    // KVM_IRQ_LINE ioctl: inject level-triggered IRQ into in-kernel irqchip
    #[repr(C)]
    struct KvmIrqLevel {
        irq: u32,
        level: u32,
    }
    const KVM_IRQ_LINE: libc::c_ulong = 0x4008_AE61;

    let mut events = [epoll_event { events: 0, u64: 0 }; 4];

    while running.load(Ordering::Relaxed) {
        let nfds = unsafe { epoll_wait(epfd, events.as_mut_ptr(), events.len() as i32, 200) };
        if nfds < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            error!("vsock-irq: epoll_wait failed: {}", e);
            break;
        }

        for i in 0..nfds as usize {
            let idx = events[i].u64 as usize;
            if idx < call_fds.len() {
                // Consume the eventfd signal
                let mut buf = [0u8; 8];
                let _ = unsafe { libc::read(call_fds[idx], buf.as_mut_ptr() as *mut libc::c_void, 8) };

                // Set INTERRUPT_STATUS so the guest ISR sees a used-buffer notification
                if let Ok(mut dev) = vsock_mmio.lock() {
                    dev.set_interrupt_status(1);
                }

                // Assert IRQ 11 (level high) then deassert (level low) for edge behavior
                let assert_irq = KvmIrqLevel { irq: 11, level: 1 };
                unsafe { libc::ioctl(vm_fd, KVM_IRQ_LINE, &assert_irq); }
                let deassert_irq = KvmIrqLevel { irq: 11, level: 0 };
                unsafe { libc::ioctl(vm_fd, KVM_IRQ_LINE, &deassert_irq); }
            }
        }
    }

    unsafe { libc::close(epfd); }
    debug!("vsock-irq thread exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_output() {
        let output = ExecOutput::new(b"hello\n".to_vec(), b"error\n".to_vec(), 0);
        assert!(output.success());
        assert_eq!(output.stdout_str(), "hello\n");
        assert_eq!(output.stderr_str(), "error\n");
    }

    #[test]
    fn test_exec_output_failure() {
        let output = ExecOutput::new(vec![], b"failed\n".to_vec(), 1);
        assert!(!output.success());
    }
}
