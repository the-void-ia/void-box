//! vCPU creation, execution, and lifecycle management.
//!
//! Architecture-specific register configuration, capture, and restore are
//! delegated to [`crate::vmm::arch::CurrentArch`].

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use kvm_ioctls::VcpuExit;
use tracing::{debug, error, trace, warn};

use crate::devices::serial::SerialDevice;
use crate::devices::virtio_9p::Virtio9pDevice;
use crate::devices::virtio_blk::VirtioBlkDevice;
use crate::devices::virtio_net::VirtioNetDevice;
use crate::devices::vsock_backend::VsockMmioDevice;
use crate::vmm::arch::{self, Arch, CurrentArch};
use crate::vmm::kvm::Vm;
use crate::{Error, Result};

/// Handle to a running vCPU thread
pub struct VcpuHandle {
    thread: JoinHandle<()>,
    id: u64,
    /// vCPU state captured when the thread exits (for snapshotting).
    exit_state: Arc<Mutex<Option<arch::VcpuState>>>,
    /// Native pthread ID for signaling the vCPU thread out of KVM_RUN.
    pthread_id: Arc<std::sync::atomic::AtomicU64>,
}

impl VcpuHandle {
    /// Wait for the vCPU thread to finish
    pub fn join(self) -> Result<()> {
        self.thread
            .join()
            .map_err(|_| Error::Vcpu(format!("vCPU {} thread panicked", self.id)))
    }

    /// Wait for the vCPU thread and return captured state (for snapshotting).
    pub fn join_with_state(self) -> Result<Option<arch::VcpuState>> {
        self.thread
            .join()
            .map_err(|_| Error::Vcpu(format!("vCPU {} thread panicked", self.id)))?;
        Ok(self.exit_state.lock().unwrap().take())
    }

    /// Get a clone of the exit_state Arc for reading vCPU state during live snapshots.
    pub fn exit_state(&self) -> Arc<Mutex<Option<arch::VcpuState>>> {
        self.exit_state.clone()
    }

    /// Get a clone of the pthread_id Arc for passing to other threads.
    pub fn pthread_id(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.pthread_id.clone()
    }

    /// Send a signal to the vCPU thread to kick it out of KVM_RUN (causes EINTR).
    pub fn kick(&self) {
        let tid = self.pthread_id.load(std::sync::atomic::Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                libc::pthread_kill(tid as libc::pthread_t, libc::SIGRTMIN());
            }
        }
    }
}

/// MMIO device bundle passed into the vCPU run loop for dispatch
pub struct MmioDevices {
    pub virtio_net: Option<Arc<Mutex<VirtioNetDevice>>>,
    pub virtio_vsock: Option<Arc<Mutex<dyn VsockMmioDevice>>>,
    pub virtio_9p: Option<Arc<Mutex<Virtio9pDevice>>>,
    pub virtio_blk: Option<Arc<Mutex<VirtioBlkDevice>>>,
}

/// Create and start a vCPU with fresh register state (cold boot).
#[allow(clippy::too_many_arguments)]
pub fn create_vcpu(
    vm: Arc<Vm>,
    vcpu_id: u64,
    entry_point: u64,
    running: Arc<AtomicBool>,
    serial: SerialDevice,
    mmio_devices: MmioDevices,
) -> Result<VcpuHandle> {
    let vcpu_fd = vm.create_vcpu(vcpu_id)?;
    debug!("Created vCPU {}", vcpu_id);

    // Delegate arch-specific configuration (CPUID + regs on x86, vcpu_init on aarch64)
    CurrentArch::configure_vcpu(&vcpu_fd, vcpu_id, entry_point, &vm)?;

    // Start vCPU thread with state capture on exit
    spawn_vcpu_thread(vm, vcpu_fd, vcpu_id, running, serial, mmio_devices)
}

/// Create and start a vCPU with state restored from a snapshot.
#[allow(clippy::too_many_arguments)]
pub fn create_vcpu_restored(
    vm: Arc<Vm>,
    vcpu_id: u64,
    state: &arch::VcpuState,
    running: Arc<AtomicBool>,
    serial: SerialDevice,
    mmio_devices: MmioDevices,
) -> Result<VcpuHandle> {
    let vcpu_fd = vm.create_vcpu(vcpu_id)?;
    debug!("Created vCPU {} for restore", vcpu_id);

    // On x86, CPUID must be configured before setting registers even on restore.
    // On aarch64, vcpu_init must be called first.
    // The arch-specific configure_vcpu handles the minimal setup needed.
    // Then we overlay the snapshot state.
    #[cfg(target_arch = "x86_64")]
    {
        // x86 needs CPUID set before any register restore
        crate::vmm::arch::x86_64::cpu::configure_vcpu(&vcpu_fd, vcpu_id, 0, &vm)?;
    }

    // Restore full register state from snapshot
    CurrentArch::restore_vcpu_state(&vcpu_fd, state, vcpu_id)?;

    // Start vCPU thread with state capture on exit
    spawn_vcpu_thread(vm, vcpu_fd, vcpu_id, running, serial, mmio_devices)
}

/// Install a no-op signal handler for SIGRTMIN so that `pthread_kill(SIGRTMIN)`
/// interrupts KVM_RUN with EINTR instead of terminating the process.
///
/// This is idempotent — safe to call multiple times.
pub fn install_vcpu_signal_handler() {
    extern "C" fn noop_handler(_: libc::c_int) {}
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = noop_handler as *const () as usize;
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGRTMIN(), &sa, std::ptr::null_mut());
    }
}

fn spawn_vcpu_thread(
    vm: Arc<Vm>,
    vcpu_fd: kvm_ioctls::VcpuFd,
    vcpu_id: u64,
    running: Arc<AtomicBool>,
    serial: SerialDevice,
    mmio_devices: MmioDevices,
) -> Result<VcpuHandle> {
    let exit_state: Arc<Mutex<Option<arch::VcpuState>>> = Arc::new(Mutex::new(None));
    let exit_state_clone = exit_state.clone();
    let pthread_id = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let pthread_id_clone = pthread_id.clone();

    let thread = thread::Builder::new()
        .name(format!("vcpu-{}", vcpu_id))
        .spawn(move || {
            // Store our pthread ID so the host can signal us out of KVM_RUN.
            pthread_id_clone.store(
                unsafe { libc::pthread_self() } as u64,
                std::sync::atomic::Ordering::SeqCst,
            );
            vcpu_run_loop(
                vcpu_fd,
                vcpu_id,
                running,
                serial,
                vm,
                mmio_devices,
                exit_state_clone,
            );
        })
        .map_err(|e| Error::Vcpu(format!("Failed to spawn vCPU thread: {}", e)))?;

    Ok(VcpuHandle {
        thread,
        id: vcpu_id,
        exit_state,
        pthread_id,
    })
}

/// vCPU run loop - executes vCPU and handles VM exits
#[allow(clippy::too_many_arguments)]
fn vcpu_run_loop(
    mut vcpu_fd: kvm_ioctls::VcpuFd,
    vcpu_id: u64,
    running: Arc<AtomicBool>,
    mut serial: SerialDevice,
    vm: Arc<Vm>,
    mmio_devices: MmioDevices,
    exit_state: Arc<Mutex<Option<arch::VcpuState>>>,
) {
    debug!("vCPU {} entering run loop", vcpu_id);
    let guest_memory = vm.guest_memory();
    let mut p9_irq_notified = false;
    let mut blk_irq_notified = false;
    let mut exit_count: u64 = 0;
    let mut hlt_count: u64 = 0;

    // Block SIGRTMIN on this thread so it can only be delivered during KVM_RUN
    // via KVM_SET_SIGNAL_MASK.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGRTMIN());
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());

        set_kvm_signal_mask(vcpu_fd.as_raw_fd());
    }

    while running.load(Ordering::SeqCst) {
        // Device polling/IRQ injection is handled by vCPU0 only.
        if vcpu_id == 0 {
            if let Some(ref dev) = mmio_devices.virtio_9p {
                let guard = dev.lock().unwrap();
                let pending = guard.has_pending_interrupt();
                if pending && !p9_irq_notified {
                    inject_irq(vm.vm_fd().as_raw_fd(), 12);
                    p9_irq_notified = true;
                } else if !pending {
                    p9_irq_notified = false;
                }
            }

            if let Some(ref dev) = mmio_devices.virtio_blk {
                let guard = dev.lock().unwrap();
                let pending = guard.has_pending_interrupt();
                if pending && !blk_irq_notified {
                    inject_irq(vm.vm_fd().as_raw_fd(), 13);
                    blk_irq_notified = true;
                } else if !pending {
                    blk_irq_notified = false;
                }
            }
        }

        match vcpu_fd.run() {
            Ok(exit_reason) => {
                exit_count += 1;
                trace!("vCPU {} exit: {:?}", vcpu_id, exit_reason);

                match exit_reason {
                    VcpuExit::IoOut(port, data) => {
                        handle_io_out(port, data, &mut serial);
                    }
                    VcpuExit::IoIn(port, data) => {
                        handle_io_in(port, data, &serial);
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        let handled = if let Some(ref dev) = mmio_devices.virtio_net {
                            let guard = dev.lock().unwrap();
                            if guard.handles_mmio(addr) {
                                let offset = addr - guard.mmio_base();
                                guard.mmio_read(offset, data);
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        let handled = handled
                            || if let Some(ref dev) = mmio_devices.virtio_vsock {
                                let guard = dev.lock().unwrap();
                                if guard.handles_mmio(addr) {
                                    let offset = addr - guard.mmio_base();
                                    guard.mmio_read(offset, data);
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                        let handled = handled
                            || if let Some(ref dev) = mmio_devices.virtio_blk {
                                let guard = dev.lock().unwrap();
                                if guard.handles_mmio(addr) {
                                    let offset = addr - guard.mmio_base();
                                    guard.mmio_read(offset, data);
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                        if !handled {
                            if let Some(ref dev) = mmio_devices.virtio_9p {
                                let guard = dev.lock().unwrap();
                                if guard.handles_mmio(addr) {
                                    let offset = addr - guard.mmio_base();
                                    guard.mmio_read(offset, data);
                                } else {
                                    data.iter_mut().for_each(|b| *b = 0);
                                }
                            } else {
                                data.iter_mut().for_each(|b| *b = 0);
                            }
                        }
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        let handled = if let Some(ref dev) = mmio_devices.virtio_net {
                            let mut guard = dev.lock().unwrap();
                            if guard.handles_mmio(addr) {
                                let offset = addr - guard.mmio_base();
                                guard.mmio_write(offset, data, Some(guest_memory));
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        let handled = handled
                            || if let Some(ref dev) = mmio_devices.virtio_vsock {
                                let mut guard = dev.lock().unwrap();
                                if guard.handles_mmio(addr) {
                                    let offset = addr - guard.mmio_base();
                                    if let Err(e) = guard.mmio_write(offset, data, guest_memory) {
                                        debug!("virtio-vsock MMIO write error: {}", e);
                                    }
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                        let handled = handled
                            || if let Some(ref dev) = mmio_devices.virtio_blk {
                                let mut guard = dev.lock().unwrap();
                                if guard.handles_mmio(addr) {
                                    let offset = addr - guard.mmio_base();
                                    guard.mmio_write(offset, data, Some(guest_memory));
                                    if guard.has_pending_interrupt() {
                                        inject_irq(vm.vm_fd().as_raw_fd(), 13);
                                    }
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                        if !handled {
                            if let Some(ref dev) = mmio_devices.virtio_9p {
                                let mut guard = dev.lock().unwrap();
                                if guard.handles_mmio(addr) {
                                    let offset = addr - guard.mmio_base();
                                    guard.mmio_write(offset, data, Some(guest_memory));

                                    if guard.has_pending_interrupt() {
                                        inject_irq(vm.vm_fd().as_raw_fd(), 12);
                                    }
                                }
                            }
                        }
                    }
                    VcpuExit::Hlt => {
                        hlt_count += 1;
                        debug!("vCPU {} halted (count={})", vcpu_id, hlt_count);
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    VcpuExit::Shutdown => {
                        debug!("vCPU {} shutdown", vcpu_id);
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                    VcpuExit::FailEntry(hardware_entry_failure_reason, _) => {
                        error!(
                            "vCPU {} failed entry: {:#x}",
                            vcpu_id, hardware_entry_failure_reason
                        );
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                    VcpuExit::InternalError => {
                        error!("vCPU {} internal error", vcpu_id);
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                    exit => {
                        warn!("vCPU {} unhandled exit: {:?}", vcpu_id, exit);
                    }
                }
            }
            Err(e) => {
                if e.errno() == libc::EINTR {
                    debug!("vCPU {} interrupted (EINTR), exits={}", vcpu_id, exit_count);
                    continue;
                }
                error!("vCPU {} run error: {}", vcpu_id, e);
                running.store(false, Ordering::SeqCst);
                break;
            }
        }
    }

    // Capture vCPU state before exiting (for snapshot support).
    match CurrentArch::capture_vcpu_state(&vcpu_fd) {
        Ok(state) => {
            *exit_state.lock().unwrap() = Some(state);
        }
        Err(e) => {
            debug!("vCPU {}: state capture failed (non-fatal): {}", vcpu_id, e);
        }
    }

    debug!("vCPU {} exiting run loop", vcpu_id);
}

pub(crate) fn inject_irq(vm_fd: i32, irq: u32) {
    #[repr(C)]
    struct KvmIrqLevel {
        irq: u32,
        level: u32,
    }
    const KVM_IRQ_LINE: libc::c_ulong = 0x4008_AE61;
    let assert = KvmIrqLevel { irq, level: 1 };
    unsafe {
        libc::ioctl(vm_fd, KVM_IRQ_LINE, &assert);
    }
    let deassert = KvmIrqLevel { irq, level: 0 };
    unsafe {
        libc::ioctl(vm_fd, KVM_IRQ_LINE, &deassert);
    }
}

/// Set the KVM signal mask on a vCPU fd so that SIGRTMIN is NOT blocked
/// during KVM_RUN.
unsafe fn set_kvm_signal_mask(vcpu_fd: i32) {
    const KVM_SET_SIGNAL_MASK: libc::c_ulong = 0x4004_AE8B;

    #[repr(C)]
    struct KvmSignalMask {
        len: u32,
        sigset: [u8; 8],
    }

    let mask = KvmSignalMask {
        len: 8,
        sigset: [0u8; 8],
    };
    libc::ioctl(vcpu_fd, KVM_SET_SIGNAL_MASK, &mask);
}

/// Handle I/O port output (guest writing to port)
fn handle_io_out(port: u16, data: &[u8], serial: &mut SerialDevice) {
    if (0x3f8..=0x3ff).contains(&port) {
        let offset = port - 0x3f8;
        for &byte in data {
            serial.write(offset as u8, byte);
        }
    } else if port == 0x64 && data.first() == Some(&0xFE) {
        debug!("Guest wrote 0xFE to port 0x64 (reboot via KB controller)");
    } else {
        trace!("Unhandled IO out: port={:#x}, data={:?}", port, data);
    }
}

/// Handle I/O port input (guest reading from port)
fn handle_io_in(port: u16, data: &mut [u8], serial: &SerialDevice) {
    if (0x3f8..=0x3ff).contains(&port) {
        let offset = port - 0x3f8;
        for byte in data {
            *byte = serial.read(offset as u8);
        }
    } else if port == 0x64 {
        data.iter_mut().for_each(|b| *b = 0x00);
    } else {
        trace!("Unhandled IO in: port={:#x}", port);
        data.iter_mut().for_each(|b| *b = 0xFF);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_segment_constants() {
        // Verified in arch/x86_64/cpu.rs tests
    }
}
