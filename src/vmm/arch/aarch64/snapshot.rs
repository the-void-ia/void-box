//! aarch64 snapshot types.

use serde::{Deserialize, Serialize};

/// Serializable vCPU state for aarch64.
///
/// All registers are captured/restored via `KVM_GET_ONE_REG` / `KVM_SET_ONE_REG`.
/// Each pair is `(register_id, value)` where register_id follows the KVM
/// `KVM_REG_ARM64_*` encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuState {
    /// Core registers: x0–x30, SP, PC, PSTATE.
    pub core_regs: Vec<(u64, u64)>,
    /// System registers: SCTLR_EL1, TCR_EL1, TTBR0/1_EL1, MAIR_EL1, VBAR_EL1, etc.
    pub system_regs: Vec<(u64, u64)>,
    /// Floating-point / SIMD registers: V0–V31 (128-bit each), FPCR, FPSR.
    /// Each entry is `(reg_id, raw_bytes)` — 128-bit regs are 16 bytes.
    pub fp_regs: Vec<(u64, Vec<u8>)>,
    /// Timer registers: CNTVCT_EL0, CNTV_CTL_EL0, CNTV_CVAL_EL0.
    pub timer_regs: Vec<(u64, u64)>,
    /// MP state (KVM_MP_STATE_*).
    pub mp_state: Option<u32>,
}

/// Interrupt controller (GIC) state for aarch64.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrqchipState {
    /// GIC distributor registers: `(offset, value)` pairs.
    pub gic_dist_regs: Vec<(u64, u32)>,
    /// GIC redistributor registers, one Vec per vCPU.
    pub gic_redist_regs: Vec<Vec<(u64, u32)>>,
    /// GIC CPU interface registers (if GICv2), one Vec per vCPU.
    pub gic_cpu_regs: Vec<Vec<(u64, u32)>>,
}

/// aarch64-specific VM state.
///
/// On ARM there is no PIT or KVM clock equivalent, so this is empty.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArchVmState {}
