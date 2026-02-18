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
use crate::guest::protocol::{
    ExecOutputChunk, ExecRequest, ExecResponse, MkdirPRequest, MkdirPResponse, WriteFileRequest,
    WriteFileResponse,
};
use crate::network::slirp::SlirpStack;
use crate::observe::telemetry::TelemetryAggregator;
use crate::observe::Observer;
use crate::vmm::cpu::MmioDevices;
use crate::{Error, ExecOutput, Result};

use self::config::VoidBoxConfig;
use self::cpu::VcpuHandle;
use self::kvm::Vm;

/// Main MicroVm instance representing a running micro-VM
pub struct MicroVm {
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
    /// Handle to the network polling thread (SLIRP RX relay)
    net_poll_handle: Option<JoinHandle<()>>,
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
    /// Execute a command with streaming output chunks
    ExecStreaming {
        request: ExecRequest,
        response_tx: oneshot::Sender<Result<ExecResponse>>,
        chunk_tx: mpsc::Sender<ExecOutputChunk>,
    },
    /// Start a telemetry subscription
    SubscribeTelemetry {
        aggregator: Arc<TelemetryAggregator>,
    },
    /// Stop the VM
    Stop,
}

impl MicroVm {
    /// Create and start a new micro-VM with the given configuration
    pub async fn new(config: VoidBoxConfig) -> Result<Self> {
        info!("Creating new MicroVm with config: {:?}", config);

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
            Some(Arc::new(VsockDevice::with_secret(
                cid,
                config.security.session_secret,
            )?))
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
                    return Err(Error::Device(format!(
                        "vsock requested but virtio-vsock MMIO backend failed to initialize: {}. \
Ensure /dev/vhost-vsock exists (e.g. modprobe vhost_vsock) and the runner supports vhost-vsock.",
                        e
                    )));
                }
            }
        } else {
            None
        };

        // Virtio-net with SLIRP backend if networking is enabled
        let virtio_net = if config.network {
            debug!("Setting up SLIRP networking");
            let slirp = Arc::new(Mutex::new(SlirpStack::with_security(
                config.security.max_concurrent_connections,
                config.security.max_connections_per_second,
                &config.security.network_deny_list,
            )?));
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

        // Spawn a background thread that polls SLIRP for host→guest TCP data
        // independently of the vCPU.  Without this, data from host sockets can
        // sit unread for seconds while the guest is doing computation (Node.js
        // startup, V8 JIT) because KVM_RUN doesn't exit during pure computation.
        let net_poll_handle = if let Some(ref net_dev) = mmio_devices.virtio_net {
            let net_dev_clone = net_dev.clone();
            let vm_clone2 = vm.clone();
            let running_net = running.clone();
            let handle = std::thread::Builder::new()
                .name("net-poll".into())
                .spawn(move || {
                    net_poll_thread(net_dev_clone, vm_clone2, running_net);
                })
                .expect("Failed to spawn net-poll thread");
            debug!("Spawned net-poll thread for SLIRP RX relay");
            Some(handle)
        } else {
            None
        };

        // Create command channel
        let (command_tx, mut command_rx) = mpsc::channel::<VmCommand>(32);

        // Start VM event loop
        let running_clone = running.clone();
        let vsock_clone = vsock.clone();
        let enable_seccomp = config.security.seccomp;
        let event_loop_handle = std::thread::spawn(move || {
            // Install seccomp-BPF filter after all setup is done.
            // This restricts the VMM process to only the syscalls needed for
            // KVM operation, limiting blast radius of a hypothetical KVM escape.
            if enable_seccomp {
                if let Err(e) = install_seccomp_filter() {
                    error!(
                        "Failed to install seccomp filter: {} (continuing without seccomp)",
                        e
                    );
                }
            }

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
                                VmCommand::ExecStreaming { request, response_tx, chunk_tx } => {
                                    let result = if let Some(ref vsock) = vsock_clone {
                                        vsock.send_exec_request_streaming(&request, |chunk| {
                                            let _ = chunk_tx.try_send(chunk);
                                        }).await
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

        // Drop all capabilities after VM setup is complete.
        // This limits what a compromised VMM process can do.
        // PR_SET_NO_NEW_PRIVS prevents gaining new privileges via execve.
        unsafe {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        }
        debug!("Set PR_SET_NO_NEW_PRIVS");

        info!(
            "MicroVm started with CID {}, network={}",
            cid, config.network
        );

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
            net_poll_handle,
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
        self.exec_with_env_timeout(program, args, stdin, env, working_dir, None)
            .await
    }

    /// Like `exec_with_env` but with an optional per-request timeout that overrides
    /// the default vsock read timeout.
    pub async fn exec_with_env_timeout(
        &self,
        program: &str,
        args: &[&str],
        stdin: &[u8],
        env: &[(String, String)],
        working_dir: Option<&str>,
        timeout_secs: Option<u64>,
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
            timeout_secs,
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

    /// Execute a command with streaming output.
    ///
    /// Returns a channel of `ExecOutputChunk` messages (stdout/stderr chunks as
    /// they're produced) and a oneshot receiver for the final `ExecResponse`.
    /// The final response still contains the complete accumulated output.
    pub async fn exec_streaming(
        &self,
        program: &str,
        args: &[&str],
        env: &[(String, String)],
        working_dir: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<(
        mpsc::Receiver<ExecOutputChunk>,
        oneshot::Receiver<Result<ExecResponse>>,
    )> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(Error::VmNotRunning);
        }

        let mut exec_env = env.to_vec();
        if let Some(ref ctx) = self.active_span_context {
            if !exec_env.iter().any(|(k, _)| k == "TRACEPARENT") {
                exec_env.push(("TRACEPARENT".to_string(), ctx.to_traceparent()));
            }
        }

        let request = ExecRequest {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdin: Vec::new(),
            env: exec_env,
            working_dir: working_dir.map(String::from),
            timeout_secs,
        };

        let (chunk_tx, chunk_rx) = mpsc::channel(256);
        let (response_tx, response_rx) = oneshot::channel();

        self.command_tx
            .send(VmCommand::ExecStreaming {
                request,
                response_tx,
                chunk_tx,
            })
            .await
            .map_err(|_| Error::Guest("Failed to send streaming command".into()))?;

        Ok((chunk_rx, response_rx))
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
    pub async fn start_telemetry(
        &mut self,
        observer: Observer,
    ) -> Result<Arc<TelemetryAggregator>> {
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

        info!("Stopping MicroVm");

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
            handle
                .join()
                .map_err(|_| Error::Vcpu("Event loop panic".into()))?;
        }

        // Wait for vsock IRQ handler if present
        if let Some(handle) = self.vsock_irq_handle.take() {
            handle
                .join()
                .map_err(|_| Error::Vcpu("vsock-irq thread panic".into()))?;
        }

        // Wait for net-poll thread if present
        if let Some(handle) = self.net_poll_handle.take() {
            handle
                .join()
                .map_err(|_| Error::Vcpu("net-poll thread panic".into()))?;
        }

        info!("MicroVm stopped");
        Ok(())
    }
}

impl Drop for MicroVm {
    fn drop(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            self.running.store(false, Ordering::SeqCst);
            error!("MicroVm dropped while still running - forcing stop");
        }
    }
}

/// Install a seccomp-bpf filter that restricts the VMM thread to the minimum
/// set of syscalls needed for KVM operation, vsock, and networking.
///
/// Uses `SECCOMP_RET_KILL_THREAD` for any disallowed syscall, terminating
/// only the event-loop thread (not the entire process). This is essential
/// for daemon mode where the parent process must survive VM teardown.
fn install_seccomp_filter() -> Result<()> {
    use seccompiler::{SeccompAction, SeccompFilter};
    use std::convert::TryInto;

    let mut rules: std::collections::BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
        std::collections::BTreeMap::new();

    // Allowlisted syscalls for KVM VMM operation
    let allowed_syscalls: &[i64] = &[
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_ioctl, // KVM ioctls
        libc::SYS_epoll_wait,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_create1,
        libc::SYS_socket, // AF_VSOCK, AF_INET
        libc::SYS_connect,
        libc::SYS_close,
        libc::SYS_clock_gettime,
        libc::SYS_nanosleep,
        libc::SYS_futex,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_exit_group,
        libc::SYS_rt_sigreturn,
        libc::SYS_recvfrom,
        libc::SYS_sendto,
        libc::SYS_accept,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_brk,
        libc::SYS_mremap,
        libc::SYS_clone, // thread creation
        libc::SYS_clone3,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_getrandom,
        libc::SYS_poll,
        libc::SYS_ppoll,
        libc::SYS_eventfd2,
        libc::SYS_openat, // for /dev/kvm, etc.
        libc::SYS_newfstatat,
        libc::SYS_fstat,
        libc::SYS_fcntl,
        libc::SYS_prctl,
        libc::SYS_seccomp, // to install this filter itself
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_tgkill,
        libc::SYS_sched_yield,
        libc::SYS_madvise,
        libc::SYS_lseek,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_writev,
        libc::SYS_readv,
        libc::SYS_sched_getaffinity,
    ];

    for &syscall in allowed_syscalls {
        // Empty rule list means "unconditional match" for this syscall.
        rules.insert(syscall, Vec::new());
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillThread, // Default: kill thread (not process) for unlisted syscalls
        SeccompAction::Allow,      // Matched rules: allow
        std::env::consts::ARCH
            .try_into()
            .map_err(|_| Error::Config("Unsupported architecture for seccomp".into()))?,
    )
    .map_err(|e| Error::Config(format!("Failed to create seccomp filter: {:?}", e)))?;

    let bpf_prog: seccompiler::BpfProgram = filter
        .try_into()
        .map_err(|e| Error::Config(format!("Failed to compile seccomp filter: {:?}", e)))?;

    seccompiler::apply_filter(&bpf_prog)
        .map_err(|e| Error::Config(format!("Failed to apply seccomp filter: {:?}", e)))?;

    info!(
        "Seccomp-BPF filter installed ({} syscalls allowed)",
        allowed_syscalls.len()
    );
    Ok(())
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
    use libc::{
        epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLIN, EPOLL_CLOEXEC, EPOLL_CTL_ADD,
    };

    let epfd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
    if epfd < 0 {
        error!(
            "vsock-irq: epoll_create1 failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    for (i, &fd) in call_fds.iter().enumerate() {
        let mut ev = epoll_event {
            events: EPOLLIN as u32,
            u64: i as u64,
        };
        let ret = unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev) };
        if ret < 0 {
            error!(
                "vsock-irq: epoll_ctl ADD fd={} failed: {}",
                fd,
                std::io::Error::last_os_error()
            );
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
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            error!("vsock-irq: epoll_wait failed: {}", e);
            break;
        }

        for event in events.iter().take(nfds as usize) {
            let idx = event.u64 as usize;
            if idx < call_fds.len() {
                // Consume the eventfd signal
                let mut buf = [0u8; 8];
                let _ =
                    unsafe { libc::read(call_fds[idx], buf.as_mut_ptr() as *mut libc::c_void, 8) };

                // Set INTERRUPT_STATUS so the guest ISR sees a used-buffer notification
                if let Ok(mut dev) = vsock_mmio.lock() {
                    dev.set_interrupt_status(1);
                }

                // Assert IRQ 11 (level high) then deassert (level low) for edge behavior
                let assert_irq = KvmIrqLevel { irq: 11, level: 1 };
                unsafe {
                    libc::ioctl(vm_fd, KVM_IRQ_LINE, &assert_irq);
                }
                let deassert_irq = KvmIrqLevel { irq: 11, level: 0 };
                unsafe {
                    libc::ioctl(vm_fd, KVM_IRQ_LINE, &deassert_irq);
                }
            }
        }
    }

    unsafe {
        libc::close(epfd);
    }
    debug!("vsock-irq thread exiting");
}

/// Background thread that polls SLIRP for host→guest TCP data.
///
/// When the guest vCPU is busy executing (e.g. Node.js JIT compilation),
/// `KVM_RUN` does not exit and the in-loop SLIRP poll never runs.  Data
/// from host TCP sockets accumulates unread, causing TLS handshakes and
/// API calls to time out.
///
/// This thread wakes every 5 ms, reads any pending host data via
/// `try_inject_rx`, and fires IRQ 10 to notify the guest.
fn net_poll_thread(net_dev: Arc<Mutex<VirtioNetDevice>>, vm: Arc<Vm>, running: Arc<AtomicBool>) {
    #[repr(C)]
    struct KvmIrqLevel {
        irq: u32,
        level: u32,
    }
    const KVM_IRQ_LINE: libc::c_ulong = 0x4008_AE61;
    let vm_fd = vm.vm_fd().as_raw_fd();
    let guest_memory = vm.guest_memory();

    while running.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(5));

        let has_interrupt = {
            let mut guard = match net_dev.lock() {
                Ok(g) => g,
                Err(_) => continue,
            };
            let _ = guard.try_inject_rx(guest_memory);
            guard.has_pending_interrupt()
        };

        if has_interrupt {
            let assert_irq = KvmIrqLevel { irq: 10, level: 1 };
            unsafe {
                libc::ioctl(vm_fd, KVM_IRQ_LINE, &assert_irq);
            }
            let deassert_irq = KvmIrqLevel { irq: 10, level: 0 };
            unsafe {
                libc::ioctl(vm_fd, KVM_IRQ_LINE, &deassert_irq);
            }
        }
    }

    debug!("net-poll thread exiting");
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
