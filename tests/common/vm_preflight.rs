#[cfg(target_os = "linux")]
use std::fs::File;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "linux")]
use kvm_ioctls::{Cap, Kvm};

#[allow(dead_code)]
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

#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub fn require_kvm_usable() -> Result<(), String> {
    require_vz_usable()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(dead_code)]
pub fn require_kvm_usable() -> Result<(), String> {
    Err("no supported VM backend on this platform".to_string())
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

#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub fn require_vsock_usable() -> Result<(), String> {
    require_vz_usable()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(dead_code)]
pub fn require_vsock_usable() -> Result<(), String> {
    Err("no supported vsock-capable VM backend on this platform".to_string())
}

#[cfg(target_os = "macos")]
fn require_vz_usable() -> Result<(), String> {
    let framework = Path::new("/System/Library/Frameworks/Virtualization.framework");
    if !framework.exists() {
        return Err(format!(
            "Virtualization.framework not found at {}",
            framework.display()
        ));
    }

    if std::env::consts::ARCH != "aarch64" {
        return Err(format!(
            "Virtualization.framework backend requires Apple Silicon; found arch {}",
            std::env::consts::ARCH
        ));
    }

    let product_version = Command::new("sw_vers")
        .args(["-productVersion"])
        .output()
        .map_err(|e| format!("failed to query macOS version via sw_vers: {e}"))?;
    if !product_version.status.success() {
        return Err("sw_vers -productVersion failed".to_string());
    }

    let version = String::from_utf8_lossy(&product_version.stdout);
    let major = version
        .trim()
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .ok_or_else(|| format!("unable to parse macOS version '{}'", version.trim()))?;
    if major < 14 {
        return Err(format!(
            "macOS {} is too old for VZ snapshot parity tests; require macOS 14+",
            version.trim()
        ));
    }

    Ok(())
}
