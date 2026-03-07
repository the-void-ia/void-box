//! vCPU configuration and execution

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread::{self, JoinHandle};

use kvm_bindings::{kvm_regs, Msrs, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{VcpuExit, VcpuFd};
use tracing::{debug, error, trace, warn};
use vm_memory::Address;

use crate::devices::serial::SerialDevice;
use crate::devices::virtio_9p::Virtio9pDevice;
use crate::devices::virtio_blk::VirtioBlkDevice;
use crate::devices::virtio_net::VirtioNetDevice;
use crate::devices::virtio_vsock_mmio::VirtioVsockMmio;
use crate::vmm::kvm::Vm;
use crate::vmm::snapshot::{kvm_struct_from_bytes, kvm_struct_to_bytes, VcpuState};
use crate::{Error, Result};

/// x86_64 segment register constants
mod x86_64 {
    /// Code segment selector
    pub const CODE_SEG_SELECTOR: u16 = 0x10;
    /// Data segment selector
    pub const DATA_SEG_SELECTOR: u16 = 0x18;
    /// Code segment type (execute/read)
    pub const CODE_SEG_TYPE: u8 = 0x0b;
    /// Data segment type (read/write)
    pub const DATA_SEG_TYPE: u8 = 0x03;

    /// CR0: Protected mode enable
    pub const CR0_PE: u64 = 1 << 0;
    /// CR0: Paging enable
    pub const CR0_PG: u64 = 1 << 31;
    /// CR4: Physical Address Extension
    pub const CR4_PAE: u64 = 1 << 5;
    /// EFER: Long Mode Enable
    pub const EFER_LME: u64 = 1 << 8;
    /// EFER: Long Mode Active
    pub const EFER_LMA: u64 = 1 << 10;
}

/// x86_64 MSR indices to capture/restore for snapshots.
const SNAPSHOT_MSR_INDICES: &[u32] = &[
    0x0000_0010, // IA32_TSC
    0x0000_0174, // IA32_SYSENTER_CS
    0x0000_0175, // IA32_SYSENTER_ESP
    0x0000_0176, // IA32_SYSENTER_EIP
    0x0000_0277, // IA32_PAT
    0xC000_0080, // IA32_EFER
    0xC000_0081, // IA32_STAR
    0xC000_0082, // IA32_LSTAR
    0xC000_0083, // IA32_CSTAR
    0xC000_0084, // IA32_FMASK
    0xC000_0100, // IA32_FS_BASE
    0xC000_0101, // IA32_GS_BASE
    0xC000_0102, // IA32_KERNEL_GS_BASE
    0xC000_0103, // IA32_TSC_AUX
];

/// Capture the full register state of a vCPU for snapshotting.
pub fn capture_vcpu_state(vcpu_fd: &VcpuFd) -> Result<VcpuState> {
    let regs = vcpu_fd.get_regs().map_err(Error::Kvm)?;
    let sregs = vcpu_fd.get_sregs().map_err(Error::Kvm)?;
    let lapic = vcpu_fd.get_lapic().map_err(Error::Kvm)?;
    let xsave = vcpu_fd.get_xsave().map_err(Error::Kvm)?;

    // Capture MSRs
    let nmsrs = SNAPSHOT_MSR_INDICES.len();
    let mut msrs =
        Msrs::new(nmsrs).map_err(|e| Error::Vcpu(format!("Msrs::new({}): {:?}", nmsrs, e)))?;
    for (i, &index) in SNAPSHOT_MSR_INDICES.iter().enumerate() {
        msrs.as_mut_slice()[i].index = index;
    }
    let read_count = vcpu_fd.get_msrs(&mut msrs).map_err(Error::Kvm)?;

    let msr_pairs: Vec<(u32, u64)> = msrs.as_slice()[..read_count]
        .iter()
        .map(|e| (e.index, e.data))
        .collect();

    debug!(
        "Captured vCPU state: regs RIP={:#x}, {} MSRs",
        regs.rip,
        msr_pairs.len()
    );

    Ok(VcpuState {
        regs: kvm_struct_to_bytes(&regs),
        sregs: kvm_struct_to_bytes(&sregs),
        lapic: kvm_struct_to_bytes(&lapic),
        xsave: kvm_struct_to_bytes(&xsave),
        msrs: msr_pairs,
    })
}

/// Restore vCPU register state from a snapshot.
pub fn restore_vcpu_state(vcpu_fd: &VcpuFd, state: &VcpuState) -> Result<()> {
    use kvm_bindings::{kvm_lapic_state, kvm_msr_entry, kvm_sregs, kvm_xsave};

    let regs: kvm_regs = kvm_struct_from_bytes(&state.regs)?;
    let sregs: kvm_sregs = kvm_struct_from_bytes(&state.sregs)?;
    let lapic: kvm_lapic_state = kvm_struct_from_bytes(&state.lapic)?;
    let xsave: kvm_xsave = kvm_struct_from_bytes(&state.xsave)?;

    vcpu_fd.set_sregs(&sregs).map_err(Error::Kvm)?;
    vcpu_fd.set_regs(&regs).map_err(Error::Kvm)?;
    vcpu_fd.set_lapic(&lapic).map_err(Error::Kvm)?;
    vcpu_fd.set_xsave(&xsave).map_err(Error::Kvm)?;

    // Restore MSRs
    if !state.msrs.is_empty() {
        let mut msrs =
            Msrs::new(state.msrs.len()).map_err(|e| Error::Vcpu(format!("Msrs::new: {:?}", e)))?;
        for (i, &(index, data)) in state.msrs.iter().enumerate() {
            msrs.as_mut_slice()[i] = kvm_msr_entry {
                index,
                data,
                ..Default::default()
            };
        }
        vcpu_fd.set_msrs(&msrs).map_err(Error::Kvm)?;
    }

    debug!("Restored vCPU state: RIP={:#x}", regs.rip);
    Ok(())
}

/// Handle to a running vCPU thread
pub struct VcpuHandle {
    thread: JoinHandle<()>,
    id: u64,
    /// vCPU state captured when the thread exits (for snapshotting).
    exit_state: Arc<Mutex<Option<VcpuState>>>,
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
    pub fn join_with_state(self) -> Result<Option<VcpuState>> {
        self.thread
            .join()
            .map_err(|_| Error::Vcpu(format!("vCPU {} thread panicked", self.id)))?;
        Ok(self.exit_state.lock().unwrap().take())
    }

    /// Get a clone of the exit_state Arc for reading vCPU state during live snapshots.
    pub fn exit_state(&self) -> Arc<Mutex<Option<VcpuState>>> {
        self.exit_state.clone()
    }

    /// Send a signal to the vCPU thread to kick it out of KVM_RUN (causes EINTR).
    pub fn kick(&self) {
        let tid = self.pthread_id.load(std::sync::atomic::Ordering::SeqCst);
        if tid != 0 {
            // SIGRTMIN+0 is safe — it won't terminate the process because
            // we register a no-op handler in `install_vcpu_signal_handler`.
            unsafe {
                libc::pthread_kill(tid as libc::pthread_t, libc::SIGRTMIN());
            }
        }
    }
}

/// MMIO device bundle passed into the vCPU run loop for dispatch
pub struct MmioDevices {
    pub virtio_net: Option<Arc<Mutex<VirtioNetDevice>>>,
    pub virtio_vsock: Option<Arc<Mutex<VirtioVsockMmio>>>,
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
    snapshot_requested: Arc<AtomicBool>,
    snapshot_barrier: Arc<Barrier>,
) -> Result<VcpuHandle> {
    let vcpu_fd = vm.create_vcpu(vcpu_id)?;
    debug!("Created vCPU {}", vcpu_id);

    // Configure CPUID
    configure_cpuid(&vm, &vcpu_fd)?;

    // Configure special registers
    configure_sregs(&vcpu_fd, vm.memory_size())?;

    // Configure general purpose registers
    configure_regs(&vcpu_fd, entry_point)?;

    // Start vCPU thread with state capture on exit
    spawn_vcpu_thread(
        vm,
        vcpu_fd,
        vcpu_id,
        running,
        serial,
        mmio_devices,
        snapshot_requested,
        snapshot_barrier,
    )
}

/// Create and start a vCPU with state restored from a snapshot.
#[allow(clippy::too_many_arguments)]
pub fn create_vcpu_restored(
    vm: Arc<Vm>,
    vcpu_id: u64,
    state: &VcpuState,
    running: Arc<AtomicBool>,
    serial: SerialDevice,
    mmio_devices: MmioDevices,
    snapshot_requested: Arc<AtomicBool>,
    snapshot_barrier: Arc<Barrier>,
) -> Result<VcpuHandle> {
    let vcpu_fd = vm.create_vcpu(vcpu_id)?;
    debug!("Created vCPU {} for restore", vcpu_id);

    // Configure CPUID (must be set before restoring registers)
    configure_cpuid(&vm, &vcpu_fd)?;

    // Restore full register state from snapshot
    restore_vcpu_state(&vcpu_fd, state)?;

    // Start vCPU thread with state capture on exit
    spawn_vcpu_thread(
        vm,
        vcpu_fd,
        vcpu_id,
        running,
        serial,
        mmio_devices,
        snapshot_requested,
        snapshot_barrier,
    )
}

/// Spawn the vCPU thread with state capture.
#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
fn spawn_vcpu_thread(
    vm: Arc<Vm>,
    vcpu_fd: VcpuFd,
    vcpu_id: u64,
    running: Arc<AtomicBool>,
    serial: SerialDevice,
    mmio_devices: MmioDevices,
    snapshot_requested: Arc<AtomicBool>,
    snapshot_barrier: Arc<Barrier>,
) -> Result<VcpuHandle> {
    let exit_state: Arc<Mutex<Option<VcpuState>>> = Arc::new(Mutex::new(None));
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
                snapshot_requested,
                snapshot_barrier,
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

/// Configure CPUID for the vCPU
fn configure_cpuid(vm: &Vm, vcpu_fd: &VcpuFd) -> Result<()> {
    let mut cpuid = vm
        .kvm()
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(Error::Kvm)?;

    // Customize CPUID entries if needed
    // For now, use the host-supported values
    for entry in cpuid.as_mut_slice().iter_mut() {
        if entry.function == 1 {
            // Clear hypervisor bit if desired (optional)
            // entry.ecx &= !(1 << 31);
        }
    }

    vcpu_fd.set_cpuid2(&cpuid).map_err(Error::Kvm)?;
    debug!("Configured CPUID");

    Ok(())
}

/// Configure special registers (segment registers, control registers, etc.)
fn configure_sregs(vcpu_fd: &VcpuFd, _memory_size: u64) -> Result<()> {
    let mut sregs = vcpu_fd.get_sregs().map_err(Error::Kvm)?;

    // Set up code segment
    sregs.cs.base = 0;
    sregs.cs.limit = 0xFFFF_FFFF;
    sregs.cs.selector = x86_64::CODE_SEG_SELECTOR;
    sregs.cs.type_ = x86_64::CODE_SEG_TYPE;
    sregs.cs.present = 1;
    sregs.cs.dpl = 0;
    sregs.cs.db = 0;
    sregs.cs.s = 1;
    sregs.cs.l = 1; // Long mode
    sregs.cs.g = 1;

    // Set up data segment
    sregs.ds.base = 0;
    sregs.ds.limit = 0xFFFF_FFFF;
    sregs.ds.selector = x86_64::DATA_SEG_SELECTOR;
    sregs.ds.type_ = x86_64::DATA_SEG_TYPE;
    sregs.ds.present = 1;
    sregs.ds.dpl = 0;
    sregs.ds.db = 1;
    sregs.ds.s = 1;
    sregs.ds.l = 0;
    sregs.ds.g = 1;

    // Copy data segment to other segments
    sregs.es = sregs.ds;
    sregs.fs = sregs.ds;
    sregs.gs = sregs.ds;
    sregs.ss = sregs.ds;

    // Set up control registers for 64-bit mode
    sregs.cr0 = x86_64::CR0_PE | x86_64::CR0_PG;
    sregs.cr4 = x86_64::CR4_PAE;
    sregs.efer = x86_64::EFER_LME | x86_64::EFER_LMA;

    // Set up page tables (identity mapping for simplicity)
    // The kernel will set up its own page tables, but we need initial ones
    // for the transition to long mode
    sregs.cr3 = setup_page_tables_address();

    vcpu_fd.set_sregs(&sregs).map_err(Error::Kvm)?;
    debug!("Configured special registers");

    Ok(())
}

/// Configure general purpose registers
fn configure_regs(vcpu_fd: &VcpuFd, entry_point: u64) -> Result<()> {
    let regs = kvm_regs {
        rip: entry_point,
        rsp: 0,
        rsi: crate::vmm::kvm::layout::BOOT_PARAMS_ADDR.raw_value(),
        rflags: 0x2,
        ..Default::default()
    };

    vcpu_fd.set_regs(&regs).map_err(Error::Kvm)?;
    debug!(
        "Configured registers: RIP={:#x}, RSI={:#x}",
        regs.rip, regs.rsi
    );

    Ok(())
}

/// Get the address where initial page tables are set up
fn setup_page_tables_address() -> u64 {
    // The linux-loader crate sets up identity-mapped page tables
    // at a specific location. For bzImage loading, it's typically
    // set up by the boot protocol.
    // For direct kernel loading, we use 0x9000 (following Firecracker's convention)
    0x9000
}

/// vCPU run loop - executes vCPU and handles VM exits
#[allow(clippy::too_many_arguments)]
fn vcpu_run_loop(
    mut vcpu_fd: VcpuFd,
    vcpu_id: u64,
    running: Arc<AtomicBool>,
    mut serial: SerialDevice,
    vm: Arc<Vm>,
    mmio_devices: MmioDevices,
    exit_state: Arc<Mutex<Option<VcpuState>>>,
    snapshot_requested: Arc<AtomicBool>,
    snapshot_barrier: Arc<Barrier>,
) {
    debug!("vCPU {} entering run loop", vcpu_id);
    let guest_memory = vm.guest_memory();
    let mut p9_irq_notified = false;
    let mut blk_irq_notified = false;

    // Block SIGRTMIN on this thread so it can only be delivered during KVM_RUN
    // via KVM_SET_SIGNAL_MASK. This eliminates the race between checking the
    // `running` flag and entering `vcpu_fd.run()`.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGRTMIN());
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());

        // Tell KVM to unblock SIGRTMIN during KVM_RUN. KVM_SET_SIGNAL_MASK
        // specifies signals to *block* during KVM_RUN — we pass an empty set
        // so nothing extra is blocked, allowing the (thread-blocked) SIGRTMIN
        // to be delivered only inside KVM_RUN.
        set_kvm_signal_mask(vcpu_fd.as_raw_fd());
    }

    while running.load(Ordering::SeqCst) {
        // Live snapshot gate: if the host requested a snapshot, capture
        // this vCPU's state and wait at the barrier until the host has
        // finished capturing VM-level state + memory.
        if snapshot_requested.load(Ordering::SeqCst) {
            match capture_vcpu_state(&vcpu_fd) {
                Ok(state) => {
                    *exit_state.lock().unwrap() = Some(state);
                }
                Err(e) => {
                    debug!(
                        "vCPU {}: live snapshot state capture failed: {}",
                        vcpu_id, e
                    );
                }
            }
            // Wait for host to finish capturing state
            snapshot_barrier.wait();
            // Wait for host to signal resume
            snapshot_barrier.wait();
            continue;
        }
        // Device polling/IRQ injection is handled by vCPU0 only to avoid
        // duplicate IRQ storms from multiple vCPU threads.
        if vcpu_id == 0 {
            // Edge-inject IRQ 12 for virtio-9p.
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

            // Edge-inject IRQ 13 for virtio-blk.
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

                                    // Inject IRQ 12 (virtio-9p) if the device has
                                    // a pending interrupt after processing the request.
                                    if guard.has_pending_interrupt() {
                                        inject_irq(vm.vm_fd().as_raw_fd(), 12);
                                    }
                                }
                            }
                        }
                    }
                    VcpuExit::Hlt => {
                        debug!("vCPU {} halted", vcpu_id);
                        // Yield briefly so the host networking thread can make
                        // progress (SLIRP, vsock).  1 ms keeps latency low while
                        // avoiding a busy-spin.
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
                    // Interrupted, check if we should stop
                    continue;
                }
                error!("vCPU {} run error: {}", vcpu_id, e);
                running.store(false, Ordering::SeqCst);
                break;
            }
        }
    }

    // Capture vCPU state before exiting (for snapshot support).
    // This is a best-effort capture — if it fails (e.g., due to a crash),
    // the exit_state will simply remain None.
    match capture_vcpu_state(&vcpu_fd) {
        Ok(state) => {
            *exit_state.lock().unwrap() = Some(state);
        }
        Err(e) => {
            debug!("vCPU {}: state capture failed (non-fatal): {}", vcpu_id, e);
        }
    }

    debug!("vCPU {} exiting run loop", vcpu_id);
}

fn inject_irq(vm_fd: i32, irq: u32) {
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
/// during KVM_RUN. Combined with blocking SIGRTMIN at the thread level via
/// `pthread_sigmask`, this ensures SIGRTMIN is only delivered inside KVM_RUN.
unsafe fn set_kvm_signal_mask(vcpu_fd: i32) {
    // KVM_SET_SIGNAL_MASK ioctl takes a kvm_signal_mask struct followed by
    // a sigset. We pass an empty sigset (no signals blocked during KVM_RUN).
    const KVM_SET_SIGNAL_MASK: libc::c_ulong = 0x4004_AE8B;

    #[repr(C)]
    struct KvmSignalMask {
        len: u32,
        sigset: [u8; 8], // 64-bit sigset — enough for signals 1-64
    }

    let mask = KvmSignalMask {
        len: 8,
        sigset: [0u8; 8], // empty set = block nothing during KVM_RUN
    };
    libc::ioctl(vcpu_fd, KVM_SET_SIGNAL_MASK, &mask);
}

/// Handle I/O port output (guest writing to port)
fn handle_io_out(port: u16, data: &[u8], serial: &mut SerialDevice) {
    // Serial port range: 0x3f8-0x3ff (COM1)
    if (0x3f8..=0x3ff).contains(&port) {
        let offset = port - 0x3f8;
        for &byte in data {
            serial.write(offset as u8, byte);
        }
    } else {
        trace!("Unhandled IO out: port={:#x}, data={:?}", port, data);
    }
}

/// Handle I/O port input (guest reading from port)
fn handle_io_in(port: u16, data: &mut [u8], serial: &SerialDevice) {
    // Serial port range: 0x3f8-0x3ff (COM1)
    if (0x3f8..=0x3ff).contains(&port) {
        let offset = port - 0x3f8;
        for byte in data {
            *byte = serial.read(offset as u8);
        }
    } else {
        trace!("Unhandled IO in: port={:#x}", port);
        // Return 0xFF for unknown ports
        data.iter_mut().for_each(|b| *b = 0xFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_constants() {
        // Verify selectors are reasonable
        const { assert!(x86_64::CODE_SEG_SELECTOR > 0) };
        const { assert!(x86_64::DATA_SEG_SELECTOR > 0) };
        assert_ne!(x86_64::CODE_SEG_SELECTOR, x86_64::DATA_SEG_SELECTOR);
    }

    #[test]
    fn test_page_tables_address() {
        let addr = setup_page_tables_address();
        assert!(addr > 0);
        assert!(addr < 0x100000); // Below 1MB
    }
}
