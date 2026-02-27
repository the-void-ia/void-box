//! KVM backend — wraps the existing `MicroVm` behind the `VmmBackend` trait.
//!
//! This module is only compiled on Linux (`#[cfg(target_os = "linux")]`).

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info};

use crate::backend::control_channel::{ControlChannel, GuestStream};
use crate::backend::{BackendConfig, VmmBackend};
use crate::devices::virtio_vsock::VsockStream;
use crate::guest::protocol::{
    ExecOutputChunk, ExecRequest, ExecResponse, TelemetrySubscribeRequest,
};
use crate::observe::telemetry::TelemetryAggregator;
use crate::observe::tracer::SpanContext;
use crate::observe::Observer;
use crate::vmm::config::{SecurityConfig, VoidBoxConfig};
use crate::vmm::MicroVm;
use crate::{Error, ExecOutput, Result};

/// vsock port used by the guest agent.
const GUEST_AGENT_PORT: u32 = 1234;

/// Implement `GuestStream` for `VsockStream` so it can be used by `ControlChannel`.
impl GuestStream for VsockStream {
    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        VsockStream::set_read_timeout(self, timeout)
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
        }
    }
}

#[async_trait::async_trait]
impl VmmBackend for KvmBackend {
    async fn start(&mut self, config: BackendConfig) -> Result<()> {
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

        let vm = MicroVm::new(vm_config).await?;
        self.cid = vm.cid();

        // Build the control channel with AF_VSOCK connector
        let cid = self.cid;
        let session_secret = config.security.session_secret;
        let connector = Box::new(move || -> Result<Box<dyn GuestStream>> {
            let stream = VsockStream::connect(cid, GUEST_AGENT_PORT)?;
            Ok(Box::new(stream))
        });
        self.control_channel = Some(Arc::new(ControlChannel::new(connector, session_secret)));
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

        let mut exec_env = env.to_vec();
        if let Some(ref ctx) = self.span_context {
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

        let mut exec_env = env.to_vec();
        if let Some(ref ctx) = self.span_context {
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

        tokio::spawn(async move {
            let result = cc
                .send_exec_request_streaming(&request, |chunk| {
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

    async fn start_telemetry(
        &mut self,
        observer: Observer,
        opts: TelemetrySubscribeRequest,
    ) -> Result<Arc<TelemetryAggregator>> {
        let cc = self
            .control_channel
            .as_ref()
            .ok_or(Error::VmNotRunning)?
            .clone();

        let aggregator = Arc::new(TelemetryAggregator::new(observer, self.cid));
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

    fn is_running(&self) -> bool {
        self.vm.as_ref().is_some_and(|vm| vm.is_running())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(mut vm) = self.vm.take() {
            vm.stop().await?;
        }
        self.control_channel = None;
        Ok(())
    }

    fn cid(&self) -> u32 {
        self.cid
    }
}
