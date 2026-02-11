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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::devices::serial::SerialDevice;
use crate::devices::virtio_vsock::VsockDevice;
use crate::guest::protocol::{ExecRequest, ExecResponse};
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
    /// Channel to send commands to the VM event loop
    command_tx: mpsc::Sender<VmCommand>,
    /// Handle to the VM event loop thread
    event_loop_handle: Option<JoinHandle<()>>,
}

/// Commands that can be sent to the VM event loop
enum VmCommand {
    /// Execute a command in the guest
    Exec {
        request: ExecRequest,
        response_tx: oneshot::Sender<Result<ExecResponse>>,
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

        // Create vCPUs
        let running = Arc::new(AtomicBool::new(true));
        let mut vcpu_handles = Vec::with_capacity(config.vcpus);

        for vcpu_id in 0..config.vcpus {
            let handle = cpu::create_vcpu(
                vm.clone(),
                vcpu_id as u64,
                entry_point,
                running.clone(),
                serial.clone(),
            )?;
            vcpu_handles.push(handle);
        }
        debug!("Created {} vCPUs", config.vcpus);

        // Set up vsock for guest communication
        let cid = config.cid.unwrap_or_else(|| {
            // Generate a random CID (must be > 2, as 0-2 are reserved)
            use std::time::{SystemTime, UNIX_EPOCH};
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u32;
            3 + (seed % 0xFFFF_FFFC)
        });

        let vsock = if config.enable_vsock {
            Some(Arc::new(VsockDevice::new(cid)?))
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

        info!("VoidBox started with CID {}", cid);

        Ok(Self {
            vm,
            vcpu_handles,
            running,
            serial_output: serial_rx,
            cid,
            vsock,
            command_tx,
            event_loop_handle: Some(event_loop_handle),
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
        if !self.running.load(Ordering::SeqCst) {
            return Err(Error::VmNotRunning);
        }

        let request = ExecRequest {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            stdin: stdin.to_vec(),
            env: Vec::new(),
            working_dir: None,
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
