//! aarch64 vCPU configuration and snapshot capture/restore.
//!
//! Uses `KVM_GET_ONE_REG` / `KVM_SET_ONE_REG` for all register access.

use std::mem::offset_of;

use kvm_bindings::{
    kvm_regs, user_fpsimd_state, user_pt_regs, KVM_REG_ARM64, KVM_REG_ARM64_SYSREG,
    KVM_REG_ARM_CORE, KVM_REG_SIZE_U128, KVM_REG_SIZE_U32, KVM_REG_SIZE_U64,
};
use kvm_ioctls::VcpuFd;
use tracing::debug;

use crate::vmm::kvm::Vm;
use crate::{Error, Result};

use super::snapshot::VcpuState;

// KVM register IDs follow the arm64 ABI (arch/arm64/include/uapi/asm/kvm.h):
// the coproc class occupies bits 16–27 of the ID, and a core register is
// addressed by offsetof(struct kvm_regs, <field>) in 32-bit words. IDs built
// with the class bits or offsets in any other position land in the kernel's
// system-register lookup, which fails with ENOENT. Offsets are derived from
// the `kvm_bindings` struct layout so they cannot drift from the ABI.

/// Encode a core register ID from its size class and byte offset in `kvm_regs`.
const fn core_reg(size: u64, byte_offset: usize) -> u64 {
    KVM_REG_ARM64 | size | (KVM_REG_ARM_CORE as u64) | (byte_offset / 4) as u64
}

/// Encode a system register ID from its Op0/Op1/CRn/CRm/Op2 fields.
const fn sys_reg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    KVM_REG_ARM64
        | KVM_REG_SIZE_U64
        | (KVM_REG_ARM64_SYSREG as u64)
        | ((op0 & 3) << 14)
        | ((op1 & 7) << 11)
        | ((crn & 0xf) << 7)
        | ((crm & 0xf) << 3)
        | (op2 & 7)
}

/// Byte offset of general-purpose register `x<n>` within `kvm_regs`.
const fn xreg_offset(n: usize) -> usize {
    offset_of!(kvm_regs, regs) + offset_of!(user_pt_regs, regs) + n * 8
}

/// Byte offset of FP/SIMD vector register `V<i>` within `kvm_regs`.
const fn vreg_offset(i: usize) -> usize {
    offset_of!(kvm_regs, fp_regs) + offset_of!(user_fpsimd_state, vregs) + i * 16
}

const SP_OFFSET: usize = offset_of!(kvm_regs, regs) + offset_of!(user_pt_regs, sp);
const PC_OFFSET: usize = offset_of!(kvm_regs, regs) + offset_of!(user_pt_regs, pc);
const PSTATE_OFFSET: usize = offset_of!(kvm_regs, regs) + offset_of!(user_pt_regs, pstate);
const FPSR_OFFSET: usize = offset_of!(kvm_regs, fp_regs) + offset_of!(user_fpsimd_state, fpsr);
const FPCR_OFFSET: usize = offset_of!(kvm_regs, fp_regs) + offset_of!(user_fpsimd_state, fpcr);

/// Core registers to capture: x0–x30, SP, PC, PSTATE.
fn core_reg_ids() -> Vec<u64> {
    let mut ids = Vec::with_capacity(34);
    for n in 0..31 {
        ids.push(core_reg(KVM_REG_SIZE_U64, xreg_offset(n)));
    }
    ids.push(core_reg(KVM_REG_SIZE_U64, SP_OFFSET));
    ids.push(core_reg(KVM_REG_SIZE_U64, PC_OFFSET));
    ids.push(core_reg(KVM_REG_SIZE_U64, PSTATE_OFFSET));
    ids
}

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
    let pc_id = core_reg(KVM_REG_SIZE_U64, PC_OFFSET);
    vcpu_fd
        .set_one_reg(pc_id, &entry_point.to_le_bytes())
        .map_err(Error::Kvm)?;

    // Set x0 to DTB address (Linux boot protocol for ARM64).
    let dtb_addr = super::kvm::layout::DTB_ADDR;
    let x0_id = core_reg(KVM_REG_SIZE_U64, xreg_offset(0));
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
    let core_reg_ids = core_reg_ids();
    let mut core_regs = Vec::with_capacity(core_reg_ids.len());
    for reg_id in core_reg_ids {
        match get_one_reg_u64(vcpu_fd, reg_id) {
            Ok(val) => core_regs.push((reg_id, val)),
            Err(e) => {
                debug!("Core reg {:#x} not available: {}", reg_id, e);
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
    for i in 0..32 {
        let reg_id = core_reg(KVM_REG_SIZE_U128, vreg_offset(i));
        let mut buf = [0u8; 16];
        match vcpu_fd.get_one_reg(reg_id, &mut buf) {
            Ok(_) => fp_regs.push((reg_id, buf.to_vec())),
            Err(e) => {
                debug!("FP reg V{} not available: {}", i, e);
            }
        }
    }
    // FPSR and FPCR are 32-bit registers and must be encoded (and buffered)
    // as U32 — the kernel rejects a mismatched size class.
    for (name, offset) in [("FPSR", FPSR_OFFSET), ("FPCR", FPCR_OFFSET)] {
        let reg_id = core_reg(KVM_REG_SIZE_U32, offset);
        let mut buf = [0u8; 4];
        match vcpu_fd.get_one_reg(reg_id, &mut buf) {
            Ok(_) => fp_regs.push((reg_id, buf.to_vec())),
            Err(e) => {
                debug!("{} not available: {}", name, e);
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

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values are the arm64 KVM ABI register IDs as documented in
    // arch/arm64/include/uapi/asm/kvm.h (and used verbatim by Firecracker,
    // crosvm, and QEMU), pinning the offset_of!-derived encoding.

    #[test]
    fn core_reg_ids_match_abi() {
        assert_eq!(
            core_reg(KVM_REG_SIZE_U64, xreg_offset(0)),
            0x6030_0000_0010_0000
        ); // x0
        assert_eq!(
            core_reg(KVM_REG_SIZE_U64, xreg_offset(1)),
            0x6030_0000_0010_0002
        ); // x1
        assert_eq!(core_reg(KVM_REG_SIZE_U64, SP_OFFSET), 0x6030_0000_0010_003e);
        assert_eq!(core_reg(KVM_REG_SIZE_U64, PC_OFFSET), 0x6030_0000_0010_0040);
        assert_eq!(
            core_reg(KVM_REG_SIZE_U64, PSTATE_OFFSET),
            0x6030_0000_0010_0042
        );
    }

    #[test]
    fn fp_reg_ids_match_abi() {
        assert_eq!(
            core_reg(KVM_REG_SIZE_U128, vreg_offset(0)),
            0x6040_0000_0010_0054
        ); // V0
        assert_eq!(
            core_reg(KVM_REG_SIZE_U32, FPSR_OFFSET),
            0x6020_0000_0010_00d4
        );
        assert_eq!(
            core_reg(KVM_REG_SIZE_U32, FPCR_OFFSET),
            0x6020_0000_0010_00d5
        );
    }

    #[test]
    fn sys_reg_ids_match_abi() {
        // MPIDR_EL1 = Op0 3, Op1 0, CRn 0, CRm 0, Op2 5.
        assert_eq!(sys_reg(3, 0, 0, 0, 5), 0x6030_0000_0013_c005);
    }
}
