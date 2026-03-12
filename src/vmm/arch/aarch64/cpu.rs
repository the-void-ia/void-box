//! aarch64 vCPU configuration and snapshot capture/restore.
//!
//! Uses `KVM_GET_ONE_REG` / `KVM_SET_ONE_REG` for all register access.

use kvm_ioctls::VcpuFd;
use tracing::debug;

use crate::vmm::kvm::Vm;
use crate::{Error, Result};

use super::snapshot::VcpuState;

// KVM register ID encoding for ARM64.
// See: arch/arm64/include/uapi/asm/kvm.h in the Linux kernel.

const KVM_REG_ARM64: u64 = 0x6030_0000_0000_0000;
const KVM_REG_SIZE_U64: u64 = 0x0030_0000_0000_0000;
const KVM_REG_SIZE_U128: u64 = 0x0040_0000_0000_0000;
const KVM_REG_ARM_CORE: u64 = 0x0010_0000_0000_0000;
const KVM_REG_ARM64_SYSREG: u64 = 0x0013_0000_0000_0000;
const KVM_REG_ARM64_SVE: u64 = 0x0015_0000_0000_0000;

/// Encode a core register ID by its offset in `kvm_regs` (in u64 units).
const fn core_reg(offset: u64) -> u64 {
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE | (offset & 0xFFFF)
}

/// Encode a system register ID from its Op0/Op1/CRn/CRm/Op2 fields.
const fn sys_reg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    KVM_REG_ARM64
        | KVM_REG_SIZE_U64
        | KVM_REG_ARM64_SYSREG
        | ((op0 & 3) << 14)
        | ((op1 & 7) << 11)
        | ((crn & 0xf) << 7)
        | ((crm & 0xf) << 3)
        | (op2 & 7)
}

// Core register offsets (in u64 units within struct kvm_regs).
// x0 = offset 0, x1 = offset 1, ..., x30 = offset 30.
// SP = 31, PC = 32, PSTATE = 33.

/// Core registers to capture: x0–x30, SP, PC, PSTATE.
const CORE_REG_OFFSETS: &[u64] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, // SP
    32, // PC
    33, // PSTATE
];

/// System registers essential for snapshot/restore.
fn system_reg_ids() -> Vec<u64> {
    vec![
        sys_reg(3, 0, 1, 0, 0),  // SCTLR_EL1
        sys_reg(3, 0, 2, 0, 0),  // TCR_EL1
        sys_reg(3, 0, 2, 0, 1),  // TTBR0_EL1
        sys_reg(3, 0, 2, 0, 2),  // TTBR1_EL1  (Op2 differs from TTBR0)
        sys_reg(3, 0, 10, 2, 0), // MAIR_EL1
        sys_reg(3, 0, 12, 0, 0), // VBAR_EL1
        sys_reg(3, 0, 13, 0, 1), // CONTEXTIDR_EL1
        sys_reg(3, 0, 5, 1, 0),  // ESR_EL1
        sys_reg(3, 0, 5, 2, 0),  // ESR_EL1 (Instruction Fault)
        sys_reg(3, 0, 6, 0, 0),  // FAR_EL1
        sys_reg(3, 0, 10, 3, 0), // AMAIR_EL1
        sys_reg(3, 0, 14, 1, 0), // CNTKCTL_EL1
        sys_reg(3, 3, 13, 0, 2), // TPIDR_EL0
        sys_reg(3, 0, 13, 0, 4), // TPIDR_EL1
        sys_reg(3, 3, 13, 0, 3), // TPIDRRO_EL0
        sys_reg(3, 4, 1, 1, 0),  // HCR_EL2 (if accessible)
    ]
}

/// Timer registers to capture.
fn timer_reg_ids() -> Vec<u64> {
    vec![
        sys_reg(3, 3, 14, 0, 1), // CNTPCT_EL0 (physical counter)
        sys_reg(3, 3, 14, 3, 1), // CNTV_CTL_EL0 (virtual timer control)
        sys_reg(3, 3, 14, 3, 2), // CNTV_CVAL_EL0 (virtual timer comparator)
    ]
}

/// Configure a freshly-created vCPU for cold boot.
///
/// Calls `KVM_ARM_VCPU_INIT` then sets the entry point (PC) and DTB address (x0).
pub fn configure_vcpu(vcpu_fd: &VcpuFd, _vcpu_id: u64, entry_point: u64, vm: &Vm) -> Result<()> {
    // Get the preferred target for this VM.
    let mut kvi = kvm_bindings::kvm_vcpu_init::default();
    vm.vm_fd()
        .get_preferred_target(&mut kvi)
        .map_err(Error::Kvm)?;

    // Initialize the vCPU with the preferred target.
    vcpu_fd.vcpu_init(&kvi).map_err(Error::Kvm)?;
    debug!("Initialized aarch64 vCPU");

    // Set PC to kernel entry point.
    let pc_id = core_reg(32); // PC
    vcpu_fd
        .set_one_reg(pc_id, &entry_point.to_le_bytes())
        .map_err(Error::Kvm)?;

    // Set x0 to DTB address (Linux boot protocol for ARM64).
    let dtb_addr = super::kvm::layout::DTB_ADDR;
    let x0_id = core_reg(0); // x0
    vcpu_fd
        .set_one_reg(x0_id, &dtb_addr.to_le_bytes())
        .map_err(Error::Kvm)?;

    debug!(
        "Configured aarch64 vCPU: PC={:#x}, x0(DTB)={:#x}",
        entry_point, dtb_addr
    );

    Ok(())
}

/// Capture the full register state of a vCPU for snapshotting.
pub fn capture_vcpu_state(vcpu_fd: &VcpuFd) -> Result<VcpuState> {
    // Core registers
    let mut core_regs = Vec::with_capacity(CORE_REG_OFFSETS.len());
    for &offset in CORE_REG_OFFSETS {
        let reg_id = core_reg(offset);
        match get_one_reg_u64(vcpu_fd, reg_id) {
            Ok(val) => core_regs.push((reg_id, val)),
            Err(e) => {
                debug!("Core reg offset {} not available: {}", offset, e);
            }
        }
    }

    // System registers
    let mut system_regs = Vec::new();
    for reg_id in system_reg_ids() {
        match get_one_reg_u64(vcpu_fd, reg_id) {
            Ok(val) => system_regs.push((reg_id, val)),
            Err(e) => {
                debug!("System reg {:#x} not available: {}", reg_id, e);
            }
        }
    }

    // FP/SIMD registers (V0-V31 are 128-bit)
    let mut fp_regs = Vec::new();
    for i in 0u64..32 {
        // V registers are in the core area at offset 34 + i*2 (128-bit = 2 u64s)
        let reg_id = KVM_REG_ARM64 | KVM_REG_SIZE_U128 | KVM_REG_ARM_CORE | (34 + i * 2);
        let mut buf = [0u8; 16];
        match vcpu_fd.get_one_reg(reg_id, &mut buf) {
            Ok(_) => fp_regs.push((reg_id, buf.to_vec())),
            Err(e) => {
                debug!("FP reg V{} not available: {}", i, e);
            }
        }
    }
    // FPSR and FPCR
    for &offset in &[98u64, 99u64] {
        // FPSR = offset 98, FPCR = offset 99
        let reg_id = core_reg(offset);
        match get_one_reg_u64(vcpu_fd, reg_id) {
            Ok(val) => fp_regs.push((reg_id, val.to_le_bytes().to_vec())),
            Err(e) => {
                debug!("FP control reg offset {} not available: {}", offset, e);
            }
        }
    }

    // Timer registers
    let mut timer_regs = Vec::new();
    for reg_id in timer_reg_ids() {
        match get_one_reg_u64(vcpu_fd, reg_id) {
            Ok(val) => timer_regs.push((reg_id, val)),
            Err(e) => {
                debug!("Timer reg {:#x} not available: {}", reg_id, e);
            }
        }
    }

    // MP state
    let mp_state = match vcpu_fd.get_mp_state() {
        Ok(mp) => Some(mp.mp_state),
        Err(e) => {
            debug!("get_mp_state failed: {}", e);
            None
        }
    };

    debug!(
        "Captured aarch64 vCPU state: {} core, {} sys, {} fp, {} timer regs",
        core_regs.len(),
        system_regs.len(),
        fp_regs.len(),
        timer_regs.len()
    );

    Ok(VcpuState {
        core_regs,
        system_regs,
        fp_regs,
        timer_regs,
        mp_state,
    })
}

/// Restore vCPU register state from a snapshot.
///
/// Restore order: system regs → core regs → FP regs → timer regs → MP state.
pub fn restore_vcpu_state(vcpu_fd: &VcpuFd, state: &VcpuState, _vcpu_id: u64) -> Result<()> {
    // 1. System registers first (page tables, control regs)
    for &(reg_id, val) in &state.system_regs {
        if let Err(e) = vcpu_fd.set_one_reg(reg_id, &val.to_le_bytes()) {
            debug!("Failed to restore system reg {:#x}: {}", reg_id, e);
        }
    }

    // 2. Core registers (x0-x30, SP, PC, PSTATE)
    for &(reg_id, val) in &state.core_regs {
        vcpu_fd
            .set_one_reg(reg_id, &val.to_le_bytes())
            .map_err(Error::Kvm)?;
    }

    // 3. FP/SIMD registers
    for (reg_id, bytes) in &state.fp_regs {
        if let Err(e) = vcpu_fd.set_one_reg(*reg_id, bytes) {
            debug!("Failed to restore FP reg {:#x}: {}", reg_id, e);
        }
    }

    // 4. Timer registers
    for &(reg_id, val) in &state.timer_regs {
        if let Err(e) = vcpu_fd.set_one_reg(reg_id, &val.to_le_bytes()) {
            debug!("Failed to restore timer reg {:#x}: {}", reg_id, e);
        }
    }

    // 5. MP state
    if let Some(mp) = state.mp_state {
        use kvm_bindings::kvm_mp_state;
        let mp_state = kvm_mp_state { mp_state: mp };
        vcpu_fd.set_mp_state(mp_state).map_err(Error::Kvm)?;
        debug!("Restored MP state: {}", mp);
    }

    debug!("Restored aarch64 vCPU state");
    Ok(())
}

/// Helper: read a single u64 register via KVM_GET_ONE_REG.
fn get_one_reg_u64(vcpu_fd: &VcpuFd, reg_id: u64) -> Result<u64> {
    let mut buf = [0u8; 8];
    vcpu_fd.get_one_reg(reg_id, &mut buf).map_err(Error::Kvm)?;
    Ok(u64::from_le_bytes(buf))
}
