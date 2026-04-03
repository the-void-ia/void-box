//! KVM backend — wraps the existing `MicroVm` behind the `VmmBackend` trait.
//!
//! This module is only compiled on Linux (`#[cfg(target_os = "linux")]`).

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::backend::control_channel::{ControlChannel, GuestStream, GUEST_AGENT_PORT};
use crate::backend::{BackendConfig, GuestConsoleSink, VmmBackend};
use crate::devices::virtio_vsock::VsockStream;
use crate::guest::protocol::{
    build_exec_request, ExecOutputChunk, ExecResponse, PtyOpenRequest, TelemetrySubscribeRequest,
};
use crate::observe::telemetry::{TelemetryAggregator, TelemetryBuffer};
use crate::observe::tracer::SpanContext;
use crate::observe::Observer;
use crate::vmm::config::{SecurityConfig, VoidBoxConfig};
use crate::vmm::MicroVm;
use crate::{Error, ExecOutput, Result};

/// Implement `GuestStream` for `VsockStream` so it can be used by `ControlChannel`.
impl GuestStream for VsockStream {
    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        VsockStream::set_read_timeout(self, timeout)
    }

    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        std::os::unix::io::AsRawFd::as_raw_fd(self)
    }
}

/// KVM-based VM backend for Linux.
///
/// Wraps `MicroVm` and delegates all guest communication through a
/// `ControlChannel` that uses AF_VSOCK as transport.
pub struct KvmBackend {
    /// The underlying KVM micro-VM (set after `start()`).
    vm: Option<MicroVm>,
    /// Transport-agnostic control channel (set after `start()`).
    control_channel: Option<Arc<ControlChannel>>,
    /// vsock CID assigned to this VM.
    cid: u32,
    /// Active span context for TRACEPARENT propagation.
    span_context: Option<SpanContext>,
    /// Background task draining guest serial output to the configured host sink.
    guest_console_task: Option<JoinHandle<()>>,
}

impl Default for KvmBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl KvmBackend {
    /// Create a new (unstarted) KVM backend.
    pub fn new() -> Self {
        Self {
            vm: None,
            control_channel: None,
            cid: 0,
            span_context: None,
            guest_console_task: None,
        }
    }
}

fn open_guest_console_writer(sink: &GuestConsoleSink) -> Box<dyn Write + Send> {
    match sink {
        GuestConsoleSink::Disabled => Box::new(io::sink()),
        GuestConsoleSink::Stderr => Box::new(io::stderr()),
        GuestConsoleSink::File(path) => {
            if let Some(parent) = path.parent() {
                if let Err(err) = std::fs::create_dir_all(parent) {
                    warn!(
                        "KvmBackend: failed to create guest console log dir {}: {}; falling back to sink",
                        parent.display(),
                        err
                    );
                    return Box::new(io::sink());
                }
            }

            match open_guest_console_file(path) {
                Ok(file) => Box::new(file),
                Err(err) => {
                    warn!(
                        "KvmBackend: failed to open guest console log {}: {}; falling back to sink",
                        path.display(),
                        err
                    );
                    Box::new(io::sink())
                }
            }
        }
    }
}

fn open_guest_console_file(path: &Path) -> io::Result<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn spawn_guest_console_task(
    mut serial_output: mpsc::Receiver<u8>,
    sink: GuestConsoleSink,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut writer = open_guest_console_writer(&sink);
        let mut buffer = Vec::with_capacity(1024);

        while let Some(byte) = serial_output.recv().await {
            buffer.push(byte);
            while let Ok(next_byte) = serial_output.try_recv() {
                buffer.push(next_byte);
                if buffer.len() >= 1024 {
                    break;
                }
            }

            if let Err(err) = writer.write_all(&buffer) {
                warn!("KvmBackend: failed writing guest console output: {}", err);
                break;
            }
            let _ = writer.flush();
            buffer.clear();
        }
    })
}

#[async_trait::async_trait]
impl VmmBackend for KvmBackend {
    async fn start(&mut self, config: BackendConfig) -> Result<()> {
        if let Some(warning) = config.initramfs_memory_warning() {
            warn!("KvmBackend: {}", warning);
        }
        // Snapshot restore path: skip cold boot entirely
        if let Some(ref snapshot_dir) = config.snapshot {
            info!("Restoring VM from snapshot: {}", snapshot_dir.display());
            let mut vm = MicroVm::from_snapshot(snapshot_dir).await?;
            self.cid = vm.cid();

            // The session secret comes from the snapshot (baked into kernel cmdline)
            let snap = crate::vmm::snapshot::VmSnapshot::load(snapshot_dir)?;
            let session_secret: [u8; 32] = snap
                .session_secret
                .as_slice()
                .try_into()
                .map_err(|_| Error::Snapshot("invalid session secret length".into()))?;

            // Restored VMs always use the userspace vsock backend (Unix socket),
            // not AF_VSOCK (vhost).  The socket path is unique per restore instance.
            let socket_path = vm
                .vsock_socket_path()
                .expect("restored VM must have vsock socket path")
                .to_path_buf();
            let connector = Arc::new(move || -> Result<Box<dyn GuestStream>> {
                let stream = VsockStream::connect_unix(&socket_path, GUEST_AGENT_PORT)?;
                Ok(Box::new(stream))
            });
            self.control_channel = Some(Arc::new(ControlChannel::new(connector, session_secret)));
            if let Some(serial_output) = vm.take_serial_output() {
                self.guest_console_task = Some(spawn_guest_console_task(
                    serial_output,
                    config.guest_console.clone(),
                ));
            }
            self.vm = Some(vm);

            debug!("KvmBackend restored from snapshot with CID {}", self.cid);
            return Ok(());
        }

        // Normal cold-boot path
        let mut vm_config = VoidBoxConfig::new()
            .memory_mb(config.memory_mb)
            .vcpus(config.vcpus)
            .kernel(&config.kernel)
            .network(config.network)
            .enable_vsock(config.enable_vsock);

        if let Some(ref initramfs) = config.initramfs {
            vm_config = vm_config.initramfs(initramfs);
        }
        if let Some(ref rootfs) = config.rootfs {
            vm_config = vm_config.rootfs(rootfs);
        }
        if let Some(ref shared_dir) = config.shared_dir {
            vm_config = vm_config.shared_dir(shared_dir);
        }

        // Apply mounts
        vm_config.mounts = config.mounts.clone();
        vm_config.oci_rootfs = config.oci_rootfs.clone();
        vm_config.oci_rootfs_dev = config.oci_rootfs_dev.clone();
        vm_config.oci_rootfs_disk = config.oci_rootfs_disk.clone();

        // Apply security config
        vm_config.security = SecurityConfig {
            session_secret: config.security.session_secret,
            command_allowlist: config.security.command_allowlist,
            resource_limits: Default::default(),
            network_deny_list: config.security.network_deny_list,
            max_connections_per_second: config.security.max_connections_per_second,
            max_concurrent_connections: config.security.max_concurrent_connections,
            seccomp: config.security.seccomp,
        };

        let mut vm = MicroVm::new(vm_config).await?;
        self.cid = vm.cid();

        // Build the control channel with AF_VSOCK connector
        let cid = self.cid;
        let session_secret = config.security.session_secret;
        let connector = Arc::new(move || -> Result<Box<dyn GuestStream>> {
            let stream = VsockStream::connect(cid, GUEST_AGENT_PORT)?;
            Ok(Box::new(stream))
        });
        self.control_channel = Some(Arc::new(ControlChannel::new(connector, session_secret)));
        if let Some(serial_output) = vm.take_serial_output() {
            self.guest_console_task = Some(spawn_guest_console_task(
                serial_output,
                config.guest_console.clone(),
            ));
        }
        self.vm = Some(vm);

        debug!("KvmBackend started with CID {}", self.cid);
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
        let cc = self.control_channel.as_ref().ok_or(Error::VmNotRunning)?;
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
        mpsc::Receiver<ExecOutputChunk>,
        oneshot::Receiver<Result<ExecResponse>>,
    )> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or(Error::VmNotRunning)?
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

        let (chunk_tx, chunk_rx) = mpsc::channel(256);
        let (response_tx, response_rx) = oneshot::channel();

        tokio::spawn(async move {
            let result = cc
                .send_exec_request_streaming(&request, move |chunk| {
                    let _ = chunk_tx.try_send(chunk);
                })
                .await;
            let _ = response_tx.send(result);
        });

        Ok((chunk_rx, response_rx))
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        let cc = self.control_channel.as_ref().ok_or(Error::VmNotRunning)?;

        let response = cc.send_write_file(path, content).await?;
        if response.success {
            Ok(())
        } else {
            Err(Error::Guest(format!(
                "Failed to write file: {}",
                response.error.unwrap_or_default()
            )))
        }
    }

    async fn mkdir_p(&self, path: &str) -> Result<()> {
        let cc = self.control_channel.as_ref().ok_or(Error::VmNotRunning)?;

        let response = cc.send_mkdir_p(path).await?;
        if response.success {
            Ok(())
        } else {
            Err(Error::Guest(format!(
                "Failed to create directory: {}",
                response.error.unwrap_or_default()
            )))
        }
    }

    async fn file_stat(&self, path: &str) -> Result<crate::guest::protocol::FileStatResponse> {
        let cc = self.control_channel.as_ref().ok_or(Error::VmNotRunning)?;
        cc.send_file_stat(path).await
    }

    async fn read_file_native(&self, path: &str) -> Result<Vec<u8>> {
        let cc = self.control_channel.as_ref().ok_or(Error::VmNotRunning)?;
        let response = cc.send_read_file(path).await?;
        if response.success {
            Ok(response.content)
        } else {
            Err(Error::Guest(format!(
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
            .ok_or(Error::VmNotRunning)?
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
                tracing::warn!("Telemetry subscription ended: {}", e);
            }
        });

        info!("Telemetry subscription requested for CID {}", self.cid);
        Ok(aggregator)
    }

    fn set_span_context(&mut self, ctx: SpanContext) {
        self.span_context = Some(ctx);
    }

    async fn attach_pty(&self, request: PtyOpenRequest) -> Result<super::pty_session::PtySession> {
        let cc = self.control_channel.as_ref().ok_or(Error::VmNotRunning)?;
        cc.open_pty(request).await
    }

    fn is_running(&self) -> bool {
        self.vm.as_ref().is_some_and(|vm| vm.is_running())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(mut vm) = self.vm.take() {
            vm.stop().await?;
        }
        if let Some(task) = self.guest_console_task.take() {
            let _ = task.await;
        }
        self.control_channel = None;
        Ok(())
    }

    fn cid(&self) -> u32 {
        self.cid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_guest_console_writer_accepts_writes() {
        let mut writer = open_guest_console_writer(&GuestConsoleSink::Disabled);
        writer.write_all(b"discarded").unwrap();
        writer.flush().unwrap();
    }

    #[test]
    fn file_guest_console_writer_writes_to_file() {
        let tempdir = tempfile::tempdir().unwrap();
        let log_path = tempdir.path().join("guest-console.log");
        {
            let mut writer = open_guest_console_writer(&GuestConsoleSink::File(log_path.clone()));
            writer.write_all(b"hello").unwrap();
            writer.flush().unwrap();
        }
        assert_eq!(std::fs::read(&log_path).unwrap(), b"hello");
    }
}
