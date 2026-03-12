//! x86_64 snapshot types.

use serde::{Deserialize, Serialize};

/// Serializable vCPU state (raw bytes of KVM structs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpuState {
    /// `kvm_regs` as raw bytes.
    pub regs: Vec<u8>,
    /// `kvm_sregs` as raw bytes.
    pub sregs: Vec<u8>,
    /// `kvm_lapic_state` as raw bytes.
    pub lapic: Vec<u8>,
    /// `kvm_xsave` as raw bytes.
    pub xsave: Vec<u8>,
    /// MSR (index, value) pairs.
    pub msrs: Vec<(u32, u64)>,
    /// `kvm_vcpu_events` as raw bytes (interrupt/exception delivery state).
    #[serde(default)]
    pub vcpu_events: Vec<u8>,
    /// `kvm_xcrs` as raw bytes (XCR0 — controls which XSAVE features are active).
    #[serde(default)]
    pub xcrs: Vec<u8>,
    /// `kvm_mp_state` as a u32 (MP state: RUNNABLE, HALTED, etc.).
    /// Critical for SMP restore — without it, secondary vCPUs resume in
    /// wrong state (RUNNABLE instead of HALTED) causing kernel deadlocks.
    #[serde(default)]
    pub mp_state: Option<u32>,
}

/// IRQ chip state (PIC master + PIC slave + IOAPIC) as raw bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrqchipState {
    /// Raw bytes of `kvm_irqchip` with chip_id = 0 (PIC master).
    pub pic_master: Vec<u8>,
    /// Raw bytes of `kvm_irqchip` with chip_id = 1 (PIC slave).
    pub pic_slave: Vec<u8>,
    /// Raw bytes of `kvm_irqchip` with chip_id = 2 (IOAPIC).
    pub ioapic: Vec<u8>,
}

/// x86_64-specific VM state: PIT + KVM clock.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArchVmState {
    /// PIT state as raw bytes of `kvm_pit_state2`.
    pub pit: Vec<u8>,
    /// KVM clock data (`kvm_clock_data` as raw bytes) for TSC synchronization.
    pub clock: Vec<u8>,
}
