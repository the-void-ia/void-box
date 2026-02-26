use std::fs::File;
use std::path::Path;

#[cfg(target_os = "linux")]
use kvm_ioctls::{Cap, Kvm};

pub fn require_kernel_artifacts(kernel: &Path, initramfs: Option<&Path>) -> Result<(), String> {
    if !kernel.exists() {
        return Err(format!("kernel path does not exist: {}", kernel.display()));
    }
    if !kernel.is_file() {
        return Err(format!("kernel path is not a file: {}", kernel.display()));
    }
    if let Some(p) = initramfs {
        if !p.exists() {
            return Err(format!("initramfs path does not exist: {}", p.display()));
        }
        if !p.is_file() {
            return Err(format!("initramfs path is not a file: {}", p.display()));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn require_kvm_usable() -> Result<(), String> {
    if !Path::new("/dev/kvm").exists() {
        return Err("/dev/kvm not available".to_string());
    }

    let kvm = Kvm::new().map_err(|e| format!("failed to open /dev/kvm: {e}"))?;
    let api = kvm.get_api_version();
    if api < 12 {
        return Err(format!("unexpected KVM API version {api}"));
    }
    if !kvm.check_extension(Cap::Irqchip) {
        return Err("missing KVM capability: IRQCHIP".to_string());
    }
    if !kvm.check_extension(Cap::UserMemory) {
        return Err("missing KVM capability: USER_MEMORY".to_string());
    }
    kvm.create_vm()
        .map_err(|e| format!("failed to create KVM VM: {e}"))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn require_kvm_usable() -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn require_vsock_usable() -> Result<(), String> {
    let path = Path::new("/dev/vhost-vsock");
    if !path.exists() {
        return Err("/dev/vhost-vsock not available".to_string());
    }
    File::open(path).map_err(|e| format!("failed to open /dev/vhost-vsock: {e}"))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn require_vsock_usable() -> Result<(), String> {
    Ok(())
}
