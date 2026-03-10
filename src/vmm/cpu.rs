//! vCPU configuration and execution

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use kvm_bindings::{kvm_regs, Msrs, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{VcpuExit, VcpuFd};
use tracing::{debug, error, trace, warn};
use vm_memory::Address;

use crate::devices::serial::SerialDevice;
use crate::devices::virtio_9p::Virtio9pDevice;
use crate::devices::virtio_blk::VirtioBlkDevice;
use crate::devices::virtio_net::VirtioNetDevice;
use crate::devices::vsock_backend::VsockMmioDevice;
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
///
/// **Order matters**: `KVM_GET_MSRS` stops at the first unsupported index.
/// Essential MSRs come first; optional/KVM-specific ones at the end.
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
    // KVM paravirt MSRs — must come after essential ones since they may
    // not be available on all hosts. get_msrs stops at the first error.
    0x4b56_4d01, // MSR_KVM_SYSTEM_TIME_NEW (kvm-clock)
    0x4b56_4d00, // MSR_KVM_WALL_CLOCK_NEW
    0x0000_06E0, // IA32_TSC_DEADLINE (LAPIC deadline timer)
];

/// Capture the full register state of a vCPU for snapshotting.
pub fn capture_vcpu_state(vcpu_fd: &VcpuFd) -> Result<VcpuState> {
    let regs = vcpu_fd.get_regs().map_err(Error::Kvm)?;
    let sregs = vcpu_fd.get_sregs().map_err(Error::Kvm)?;
    let lapic = vcpu_fd.get_lapic().map_err(Error::Kvm)?;
    let xsave = vcpu_fd.get_xsave().map_err(Error::Kvm)?;
    let vcpu_events = vcpu_fd.get_vcpu_events().map_err(Error::Kvm)?;
    let xcrs = vcpu_fd.get_xcrs().map_err(Error::Kvm)?;

    // Capture MSRs — read each individually so unsupported ones don't
    // prevent reading subsequent MSRs (get_msrs stops at first error).
    let mut msr_pairs: Vec<(u32, u64)> = Vec::with_capacity(SNAPSHOT_MSR_INDICES.len());
    for &index in SNAPSHOT_MSR_INDICES {
        let mut msrs = Msrs::new(1).map_err(|e| Error::Vcpu(format!("Msrs::new(1): {:?}", e)))?;
        msrs.as_mut_slice()[0].index = index;
        match vcpu_fd.get_msrs(&mut msrs) {
            Ok(1) => {
                msr_pairs.push((msrs.as_slice()[0].index, msrs.as_slice()[0].data));
            }
            _ => {
                debug!("MSR {:#x} not available, skipping", index);
            }
        }
    }

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
        vcpu_events: kvm_struct_to_bytes(&vcpu_events),
        xcrs: kvm_struct_to_bytes(&xcrs),
    })
}

/// Restore vCPU register state from a snapshot.
///
/// The restore order is critical for correctness on KVM x86_64:
///
/// 1. MSRs (except IA32_TSC_DEADLINE) — TSC must be set before LAPIC
/// 2. sregs (segment registers, control registers, EFER)
/// 3. LAPIC — sets the timer mode (e.g. TSC-deadline)
/// 4. IA32_TSC_DEADLINE MSR — MUST come after LAPIC because KVM silently
///    drops the value if the LAPIC isn't in TSC-deadline mode yet
/// 5. vcpu_events (pending interrupt/exception delivery state)
/// 6. xsave (FPU/SSE/AVX state)
/// 7. regs last (general-purpose registers including RIP)
pub fn restore_vcpu_state(vcpu_fd: &VcpuFd, state: &VcpuState) -> Result<()> {
    use kvm_bindings::{
        kvm_lapic_state, kvm_msr_entry, kvm_sregs, kvm_vcpu_events, kvm_xcrs, kvm_xsave,
    };

    const IA32_TSC_DEADLINE: u32 = 0x0000_06E0;

    let regs: kvm_regs = kvm_struct_from_bytes(&state.regs)?;
    let sregs: kvm_sregs = kvm_struct_from_bytes(&state.sregs)?;
    let lapic: kvm_lapic_state = kvm_struct_from_bytes(&state.lapic)?;
    let xsave: kvm_xsave = kvm_struct_from_bytes(&state.xsave)?;

    // Restore order follows the pattern used by mature VMMs (crosvm, Firecracker):
    //
    //   1. MSRs (except TSC_DEADLINE) — TSC base must be set before LAPIC
    //   2. sregs (segment regs, CR0/CR3/CR4, EFER)
    //   3. LAPIC — timer mode must be correct before TSC_DEADLINE write
    //   4. TSC_DEADLINE MSR — MUST come after LAPIC (KVM drops it if LAPIC
    //      isn't in TSC-deadline mode yet)
    //   5. vcpu_events (pending interrupt/exception delivery state)
    //   6. xsave (FPU/SSE/AVX)
    //   7. regs last (includes RIP — execution resumes here)
    //   8. KVM_KVMCLOCK_CTRL (tells KVM guest was paused)
    //
    // Getting this order wrong is the #1 cause of "lost LAPIC timer" bugs
    // in Rust VMMs.  See: https://lkml.rescloud.iu.edu/2309.1/04940.html

    // 1. MSRs first (except TSC_DEADLINE — deferred until after LAPIC).
    let mut tsc_deadline_value: Option<u64> = None;
    for &(index, data) in &state.msrs {
        if index == IA32_TSC_DEADLINE {
            tsc_deadline_value = Some(data);
            continue; // Deferred to step 4
        }
        let mut msrs = Msrs::new(1).map_err(|e| Error::Vcpu(format!("Msrs::new: {:?}", e)))?;
        msrs.as_mut_slice()[0] = kvm_msr_entry {
            index,
            data,
            ..Default::default()
        };
        match vcpu_fd.set_msrs(&msrs) {
            Ok(_) => {}
            Err(e) => {
                debug!("Failed to restore MSR {:#x}: {}", index, e);
            }
        }
    }

    // 2. Special registers (segment regs, CR0/CR3/CR4, EFER)
    vcpu_fd.set_sregs(&sregs).map_err(Error::Kvm)?;

    // 3. LAPIC — restore faithfully, then fix LVT Timer if needed.
    //
    // When the guest is in NO_HZ idle (HLT) at snapshot time, the kernel
    // masks the LAPIC timer (LVTT = 0x10000: masked, mode=oneshot, vector=0).
    // After restore, no timer tick will ever fire, so the guest stays in HLT
    // forever — the scheduler never runs again.
    //
    // Fix: set LVT Timer to TSC-deadline mode with LOCAL_TIMER_VECTOR (0xEC)
    // and unmask it.  Then in step 4, write a near-future TSC_DEADLINE to
    // fire a single tick that bootstraps tick_nohz_idle_exit → hrtimer →
    // scheduler.  The kernel's apic_timer_interrupt handler re-arms the
    // timer and normal operation resumes.
    let mut lapic = lapic;
    {
        let lvt_timer_offset = 0x320;
        let lvt_timer = u32::from_le_bytes([
            lapic.regs[lvt_timer_offset] as u8,
            lapic.regs[lvt_timer_offset + 1] as u8,
            lapic.regs[lvt_timer_offset + 2] as u8,
            lapic.regs[lvt_timer_offset + 3] as u8,
        ]);
        let timer_masked = (lvt_timer >> 16) & 1;
        let timer_vector = lvt_timer & 0xFF;

        if timer_masked == 1 && timer_vector == 0 {
            // Guest was in NO_HZ idle — the timer is masked with vector 0.
            // Reconfigure to periodic mode with LOCAL_TIMER_VECTOR (0xEC) and
            // unmask it so the first tick bootstraps the scheduler.
            //
            // We use periodic mode (not TSC-deadline) because:
            //   - The kernel's clockevent state is stale after restore
            //   - With TSC-deadline, the kernel expects to manage the MSR and
            //     may never re-arm it after the stale clockevent fires
            //   - Periodic mode is self-sustaining: the LAPIC hardware
            //     regenerates ticks from TMICT/TDCR without kernel re-arming
            //
            // Once the kernel's timer subsystem initializes, it may switch to
            // TSC-deadline (if not disabled) and reprogram LVTT accordingly.
            let new_lvt: u32 = (0b01 << 17) | 0xEC; // Periodic, vector 0xEC, unmasked
            let bytes = new_lvt.to_le_bytes();
            lapic.regs[lvt_timer_offset] = bytes[0] as _;
            lapic.regs[lvt_timer_offset + 1] = bytes[1] as _;
            lapic.regs[lvt_timer_offset + 2] = bytes[2] as _;
            lapic.regs[lvt_timer_offset + 3] = bytes[3] as _;

            // Set TMICT (Timer Initial Count) to generate ticks.
            // With divide-by-1 (TDCR=0xB), a TMICT of ~2M at ~2GHz bus clock
            // gives roughly 1ms ticks — fast enough to bootstrap the scheduler
            // but slow enough not to overwhelm a single vCPU.
            let tmict: u32 = 0x200000; // ~2M counts
            let tmict_offset = 0x380;
            let tmict_bytes = tmict.to_le_bytes();
            lapic.regs[tmict_offset] = tmict_bytes[0] as _;
            lapic.regs[tmict_offset + 1] = tmict_bytes[1] as _;
            lapic.regs[tmict_offset + 2] = tmict_bytes[2] as _;
            lapic.regs[tmict_offset + 3] = tmict_bytes[3] as _;

            // Set TDCR (Timer Divide Configuration) to divide-by-1.
            let tdcr: u32 = 0x0B; // divide-by-1
            let tdcr_offset = 0x3E0;
            let tdcr_bytes = tdcr.to_le_bytes();
            lapic.regs[tdcr_offset] = tdcr_bytes[0] as _;
            lapic.regs[tdcr_offset + 1] = tdcr_bytes[1] as _;
            lapic.regs[tdcr_offset + 2] = tdcr_bytes[2] as _;
            lapic.regs[tdcr_offset + 3] = tdcr_bytes[3] as _;

            debug!(
                "Fixed LAPIC LVT Timer: {:#x} -> {:#x} (periodic, vector 0xEC, TMICT={:#x}, TDCR={:#x})",
                lvt_timer, new_lvt, tmict, tdcr
            );
        }
    }
    vcpu_fd.set_lapic(&lapic).map_err(Error::Kvm)?;

    // 4. TSC_DEADLINE — restore the snapshot value if non-zero.
    //    With periodic mode, this is typically unused, but restore faithfully
    //    in case the kernel was in TSC-deadline mode at snapshot time.
    if let Some(deadline) = tsc_deadline_value {
        if deadline != 0 {
            let mut msrs = Msrs::new(1).map_err(|e| Error::Vcpu(format!("Msrs::new: {:?}", e)))?;
            msrs.as_mut_slice()[0] = kvm_msr_entry {
                index: IA32_TSC_DEADLINE,
                data: deadline,
                ..Default::default()
            };
            let _ = vcpu_fd.set_msrs(&msrs);
        }
    }

    // 5. Restore vcpu_events (interrupt/exception delivery state)
    if !state.vcpu_events.is_empty() {
        let events: kvm_vcpu_events = kvm_struct_from_bytes(&state.vcpu_events)?;
        vcpu_fd.set_vcpu_events(&events).map_err(Error::Kvm)?;
    }

    // 6a. XCR0 (Extended Control Register) — must come before xsave.
    //     XCR0 controls which XSAVE features are active (x87, SSE, AVX, etc.).
    //     Without this, the guest's XRSTORS instruction will #GP because it
    //     expects features (SSE, AVX) that aren't enabled in the default XCR0.
    if !state.xcrs.is_empty() {
        let xcrs: kvm_xcrs = kvm_struct_from_bytes(&state.xcrs)?;
        vcpu_fd.set_xcrs(&xcrs).map_err(Error::Kvm)?;
        debug!("Restored XCRs ({} entries)", xcrs.nr_xcrs);
    }

    // 6b. FPU/SSE/AVX state
    vcpu_fd.set_xsave(&xsave).map_err(Error::Kvm)?;

    // 7. General-purpose registers last (includes RIP — execution resumes here)
    vcpu_fd.set_regs(&regs).map_err(Error::Kvm)?;

    // 8. KVM_KVMCLOCK_CTRL — tell KVM the guest was paused so the pvclock
    //    sets KVM_CLOCK_PAUSED.  The guest kernel reads this on resume and
    //    adjusts its timers to avoid soft lockup watchdog panics.
    if let Err(e) = vcpu_fd.kvmclock_ctrl() {
        // Not fatal — fails with EINVAL if kvm-clock is not active
        debug!("KVM_KVMCLOCK_CTRL: {} (non-fatal)", e);
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

    /// Get a clone of the pthread_id Arc for passing to other threads.
    pub fn pthread_id(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.pthread_id.clone()
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

    // Configure CPUID
    configure_cpuid(&vm, &vcpu_fd)?;

    // Configure special registers
    configure_sregs(&vcpu_fd, vm.memory_size())?;

    // Configure general purpose registers
    configure_regs(&vcpu_fd, entry_point)?;

    // Start vCPU thread with state capture on exit
    spawn_vcpu_thread(vm, vcpu_fd, vcpu_id, running, serial, mmio_devices)
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
) -> Result<VcpuHandle> {
    let vcpu_fd = vm.create_vcpu(vcpu_id)?;
    debug!("Created vCPU {} for restore", vcpu_id);

    // Configure CPUID — required by KVM before setting registers, even on restore.
    // Uses the host's supported CPUID which should match the original boot.
    configure_cpuid(&vm, &vcpu_fd)?;

    // Restore full register state from snapshot
    restore_vcpu_state(&vcpu_fd, state)?;

    // Start vCPU thread with state capture on exit
    spawn_vcpu_thread(vm, vcpu_fd, vcpu_id, running, serial, mmio_devices)
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

fn spawn_vcpu_thread(
    vm: Arc<Vm>,
    vcpu_fd: VcpuFd,
    vcpu_id: u64,
    running: Arc<AtomicBool>,
    serial: SerialDevice,
    mmio_devices: MmioDevices,
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
) {
    debug!("vCPU {} entering run loop", vcpu_id);
    let guest_memory = vm.guest_memory();
    let mut p9_irq_notified = false;
    let mut blk_irq_notified = false;
    let mut exit_count: u64 = 0;
    let mut hlt_count: u64 = 0;

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
                        hlt_count += 1;
                        debug!("vCPU {} halted (count={})", vcpu_id, hlt_count);
                        // Yield briefly so host threads can make progress.
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
                    // Interrupted by signal (e.g. SIGRTMIN for snapshot).
                    // This is normal — just re-enter KVM_RUN.
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
    } else if port == 0x64 && data.first() == Some(&0xFE) {
        // Keyboard controller reset (reboot attempt) — log it
        debug!("Guest wrote 0xFE to port 0x64 (reboot via KB controller)");
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
    } else if port == 0x64 {
        // i8042 keyboard controller status register.
        // Return 0x00: input buffer empty (bit 1=0), output buffer empty (bit 0=0).
        // Without this, the kernel's i8042 init/reset loops forever waiting for
        // the controller to become ready.
        data.iter_mut().for_each(|b| *b = 0x00);
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
