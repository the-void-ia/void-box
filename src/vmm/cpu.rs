//! vCPU configuration and execution

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use kvm_bindings::{kvm_regs, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{VcpuExit, VcpuFd};
use tracing::{debug, error, trace, warn};
use vm_memory::Address;

use crate::devices::serial::SerialDevice;
use crate::devices::virtio_net::VirtioNetDevice;
use crate::devices::virtio_vsock_mmio::VirtioVsockMmio;
use crate::vmm::kvm::Vm;
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

/// Handle to a running vCPU thread
pub struct VcpuHandle {
    thread: JoinHandle<()>,
    id: u64,
}

impl VcpuHandle {
    /// Wait for the vCPU thread to finish
    pub fn join(self) -> Result<()> {
        self.thread
            .join()
            .map_err(|_| Error::Vcpu(format!("vCPU {} thread panicked", self.id)))
    }
}

/// MMIO device bundle passed into the vCPU run loop for dispatch
pub struct MmioDevices {
    pub virtio_net: Option<Arc<Mutex<VirtioNetDevice>>>,
    pub virtio_vsock: Option<Arc<Mutex<VirtioVsockMmio>>>,
}

/// Create and start a vCPU
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

    // Start vCPU thread
    let vm_clone = vm.clone();
    let thread = thread::Builder::new()
        .name(format!("vcpu-{}", vcpu_id))
        .spawn(move || {
            vcpu_run_loop(vcpu_fd, vcpu_id, running, serial, vm_clone, mmio_devices);
        })
        .map_err(|e| Error::Vcpu(format!("Failed to spawn vCPU thread: {}", e)))?;

    Ok(VcpuHandle { thread, id: vcpu_id })
}

/// Configure CPUID for the vCPU
fn configure_cpuid(vm: &Vm, vcpu_fd: &VcpuFd) -> Result<()> {
    let mut cpuid = vm
        .kvm()
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| Error::Kvm(e))?;

    // Customize CPUID entries if needed
    // For now, use the host-supported values
    for entry in cpuid.as_mut_slice().iter_mut() {
        match entry.function {
            // Processor brand string, etc.
            1 => {
                // Clear hypervisor bit if desired (optional)
                // entry.ecx &= !(1 << 31);
            }
            _ => {}
        }
    }

    vcpu_fd.set_cpuid2(&cpuid).map_err(|e| Error::Kvm(e))?;
    debug!("Configured CPUID");

    Ok(())
}

/// Configure special registers (segment registers, control registers, etc.)
fn configure_sregs(vcpu_fd: &VcpuFd, _memory_size: u64) -> Result<()> {
    let mut sregs = vcpu_fd.get_sregs().map_err(|e| Error::Kvm(e))?;

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

    vcpu_fd.set_sregs(&sregs).map_err(|e| Error::Kvm(e))?;
    debug!("Configured special registers");

    Ok(())
}

/// Configure general purpose registers
fn configure_regs(vcpu_fd: &VcpuFd, entry_point: u64) -> Result<()> {
    let mut regs = kvm_regs::default();

    // Set instruction pointer to kernel entry
    regs.rip = entry_point;

    // Set up initial stack (kernel will set up its own)
    regs.rsp = 0;

    // Boot protocol: RSI points to boot params (zero page)
    regs.rsi = crate::vmm::kvm::layout::BOOT_PARAMS_ADDR.raw_value();

    // Flags: interrupts disabled, reserved bit 1 set
    regs.rflags = 0x2;

    vcpu_fd.set_regs(&regs).map_err(|e| Error::Kvm(e))?;
    debug!("Configured registers: RIP={:#x}, RSI={:#x}", regs.rip, regs.rsi);

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
fn vcpu_run_loop(
    mut vcpu_fd: VcpuFd,
    vcpu_id: u64,
    running: Arc<AtomicBool>,
    mut serial: SerialDevice,
    vm: Arc<Vm>,
    mmio_devices: MmioDevices,
) {
    debug!("vCPU {} entering run loop", vcpu_id);
    let guest_memory = vm.guest_memory();

    while running.load(Ordering::SeqCst) {
        // Poll virtio-net RX: inject any frames from SLIRP into guest buffers,
        // then inject IRQ 10 if the device has a pending interrupt.
        if let Some(ref dev) = mmio_devices.virtio_net {
            let mut guard = dev.lock().unwrap();
            let _ = guard.try_inject_rx(guest_memory);
            if guard.has_pending_interrupt() {
                // Inject IRQ 10 (virtio-net) into the guest via KVM_IRQ_LINE
                #[repr(C)]
                struct KvmIrqLevel { irq: u32, level: u32 }
                const KVM_IRQ_LINE: libc::c_ulong = 0x4008_AE61;
                let vm_fd = vm.vm_fd().as_raw_fd();
                let assert = KvmIrqLevel { irq: 10, level: 1 };
                unsafe { libc::ioctl(vm_fd, KVM_IRQ_LINE, &assert); }
                let deassert = KvmIrqLevel { irq: 10, level: 0 };
                unsafe { libc::ioctl(vm_fd, KVM_IRQ_LINE, &deassert); }
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
                        if !handled {
                            if let Some(ref dev) = mmio_devices.virtio_vsock {
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
                        if !handled {
                            if let Some(ref dev) = mmio_devices.virtio_vsock {
                                let mut guard = dev.lock().unwrap();
                                if guard.handles_mmio(addr) {
                                    let offset = addr - guard.mmio_base();
                                    if let Err(e) = guard.mmio_write(offset, data, guest_memory) {
                                        warn!("virtio-vsock MMIO write error: {}", e);
                                    }
                                }
                            }
                        }
                    }
                    VcpuExit::Hlt => {
                        debug!("vCPU {} halted", vcpu_id);
                        // In a real VMM, we'd wait for an interrupt
                        // For now, yield and continue
                        std::thread::sleep(std::time::Duration::from_millis(10));
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

    debug!("vCPU {} exiting run loop", vcpu_id);
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
        assert!(x86_64::CODE_SEG_SELECTOR > 0);
        assert!(x86_64::DATA_SEG_SELECTOR > 0);
        assert_ne!(x86_64::CODE_SEG_SELECTOR, x86_64::DATA_SEG_SELECTOR);
    }

    #[test]
    fn test_page_tables_address() {
        let addr = setup_page_tables_address();
        assert!(addr > 0);
        assert!(addr < 0x100000); // Below 1MB
    }
}
