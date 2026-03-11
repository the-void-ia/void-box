//! x86_64 vCPU configuration and snapshot capture/restore.

use kvm_bindings::{kvm_regs, Msrs, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::VcpuFd;
use tracing::debug;
use vm_memory::Address;

use crate::vmm::kvm::Vm;
use crate::vmm::snapshot::{kvm_struct_from_bytes, kvm_struct_to_bytes};
use crate::{Error, Result};

use super::snapshot::VcpuState;

/// x86_64 segment register constants
mod consts {
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

/// Configure a freshly-created vCPU for cold boot (CPUID + sregs + regs).
pub fn configure_vcpu(vcpu_fd: &VcpuFd, _vcpu_id: u64, entry_point: u64, vm: &Vm) -> Result<()> {
    configure_cpuid(vm, vcpu_fd)?;
    configure_sregs(vcpu_fd)?;
    configure_regs(vcpu_fd, entry_point)?;
    Ok(())
}

/// Capture the full register state of a vCPU for snapshotting.
pub fn capture_vcpu_state(vcpu_fd: &VcpuFd) -> Result<VcpuState> {
    let regs = vcpu_fd.get_regs().map_err(Error::Kvm)?;
    let sregs = vcpu_fd.get_sregs().map_err(Error::Kvm)?;
    let lapic = vcpu_fd.get_lapic().map_err(Error::Kvm)?;
    let xsave = vcpu_fd.get_xsave().map_err(Error::Kvm)?;
    let vcpu_events = vcpu_fd.get_vcpu_events().map_err(Error::Kvm)?;
    let xcrs = vcpu_fd.get_xcrs().map_err(Error::Kvm)?;
    let mp_state = vcpu_fd.get_mp_state().map_err(Error::Kvm)?;

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
        mp_state: Some(mp_state.mp_state),
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
///
/// `vcpu_id` is used to decide LAPIC timer bootstrap: only the BSP (vCPU 0)
/// gets timer bootstrap.  Secondary vCPUs are restored faithfully — the BSP
/// will IPI them when the kernel needs them.
pub fn restore_vcpu_state(vcpu_fd: &VcpuFd, state: &VcpuState, vcpu_id: u64) -> Result<()> {
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
        let timer_mode = (lvt_timer >> 17) & 0x3; // 0=oneshot, 1=periodic, 2=TSC-deadline
        let timer_vector = lvt_timer & 0xFF;

        // Read current TMICT (Timer Initial Count Register).
        let tmict_offset = 0x380;
        let current_tmict = u32::from_le_bytes([
            lapic.regs[tmict_offset] as u8,
            lapic.regs[tmict_offset + 1] as u8,
            lapic.regs[tmict_offset + 2] as u8,
            lapic.regs[tmict_offset + 3] as u8,
        ]);

        // Bootstrap the LAPIC timer if it won't fire on its own.
        //
        // IMPORTANT: Only bootstrap vCPU 0 (BSP).  Secondary vCPUs may be in
        // HALTED/INIT/SIPI state because the kernel never brought them online
        // (e.g. maxcpus=1) or they're parked in idle.  Injecting periodic
        // timer ticks into an AP whose timer subsystem was never initialized
        // causes kernel panics.  The BSP will IPI them when needed.
        let needs_bootstrap = vcpu_id == 0
            && (timer_masked == 1
                || timer_mode == 2 // TSC-deadline: stale after restore
                || (timer_mode == 0 && current_tmict == 0)); // oneshot, disarmed

        if needs_bootstrap {
            // Reconfigure to periodic mode with LOCAL_TIMER_VECTOR (0xEC)
            // and unmask it so ticks bootstrap the scheduler on every vCPU.
            //
            // Periodic mode is self-sustaining: the LAPIC hardware
            // regenerates ticks from TMICT/TDCR without kernel re-arming.
            // Once the kernel's timer subsystem re-initializes, it may
            // switch to TSC-deadline and reprogram LVTT accordingly.
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
                "LAPIC timer bootstrap: LVT {:#x} -> {:#x} (masked={}, mode={}, vec={:#x}, old_tmict={:#x})",
                lvt_timer, new_lvt, timer_masked, timer_mode, timer_vector, current_tmict
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

    // 8. MP state (RUNNABLE, HALTED, INIT_RECEIVED, SIPI_RECEIVED).
    //    Critical for SMP: without this, secondary vCPUs default to RUNNABLE
    //    instead of their actual state (usually HALTED), breaking SMP resume.
    if let Some(mp) = state.mp_state {
        use kvm_bindings::kvm_mp_state;
        let mp_state = kvm_mp_state { mp_state: mp };
        vcpu_fd.set_mp_state(mp_state).map_err(Error::Kvm)?;
        debug!("Restored MP state: {}", mp);
    }

    // 9. KVM_KVMCLOCK_CTRL — tell KVM the guest was paused so the pvclock
    //    sets KVM_CLOCK_PAUSED.  The guest kernel reads this on resume and
    //    adjusts its timers to avoid soft lockup watchdog panics.
    if let Err(e) = vcpu_fd.kvmclock_ctrl() {
        // Not fatal — fails with EINVAL if kvm-clock is not active
        debug!("KVM_KVMCLOCK_CTRL: {} (non-fatal)", e);
    }

    debug!("Restored vCPU state: RIP={:#x}", regs.rip);
    Ok(())
}

/// Configure CPUID for the vCPU.
fn configure_cpuid(vm: &Vm, vcpu_fd: &VcpuFd) -> Result<()> {
    let mut cpuid = vm
        .kvm()
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(Error::Kvm)?;

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
fn configure_sregs(vcpu_fd: &VcpuFd) -> Result<()> {
    let mut sregs = vcpu_fd.get_sregs().map_err(Error::Kvm)?;

    // Set up code segment
    sregs.cs.base = 0;
    sregs.cs.limit = 0xFFFF_FFFF;
    sregs.cs.selector = consts::CODE_SEG_SELECTOR;
    sregs.cs.type_ = consts::CODE_SEG_TYPE;
    sregs.cs.present = 1;
    sregs.cs.dpl = 0;
    sregs.cs.db = 0;
    sregs.cs.s = 1;
    sregs.cs.l = 1; // Long mode
    sregs.cs.g = 1;

    // Set up data segment
    sregs.ds.base = 0;
    sregs.ds.limit = 0xFFFF_FFFF;
    sregs.ds.selector = consts::DATA_SEG_SELECTOR;
    sregs.ds.type_ = consts::DATA_SEG_TYPE;
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
    sregs.cr0 = consts::CR0_PE | consts::CR0_PG;
    sregs.cr4 = consts::CR4_PAE;
    sregs.efer = consts::EFER_LME | consts::EFER_LMA;

    // Set up page tables (identity mapping for simplicity)
    sregs.cr3 = 0x9000;

    vcpu_fd.set_sregs(&sregs).map_err(Error::Kvm)?;
    debug!("Configured special registers");

    Ok(())
}

/// Configure general purpose registers.
fn configure_regs(vcpu_fd: &VcpuFd, entry_point: u64) -> Result<()> {
    let regs = kvm_regs {
        rip: entry_point,
        rsp: 0,
        rsi: super::kvm::layout::BOOT_PARAMS_ADDR.raw_value(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_constants() {
        const { assert!(consts::CODE_SEG_SELECTOR > 0) };
        const { assert!(consts::DATA_SEG_SELECTOR > 0) };
        assert_ne!(consts::CODE_SEG_SELECTOR, consts::DATA_SEG_SELECTOR);
    }
}
