//! Kernel loading — thin wrapper delegating to arch-specific implementation.

use std::path::Path;

use crate::vmm::arch::{Arch, CurrentArch};
use crate::vmm::kvm::Vm;
use crate::Result;

/// Load kernel and optionally initramfs into guest memory.
///
/// Delegates to the current architecture's implementation.
pub fn load_kernel(
    vm: &Vm,
    kernel_path: &Path,
    initramfs_path: Option<&Path>,
    cmdline: &str,
) -> Result<u64> {
    CurrentArch::load_kernel(vm, kernel_path, initramfs_path, cmdline)
}
