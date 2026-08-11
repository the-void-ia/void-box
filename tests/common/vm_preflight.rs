#[cfg(target_os = "linux")]
use std::fs::File;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "linux")]
use kvm_ioctls::{Cap, Kvm};

use void_box::backend::{create_backend, BackendConfig, VmmBackend};

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

/// True when `VOID_BOX_REQUIRE_VM=1`. A runner meant to boot VMs must fail,
/// not skip, when it cannot — this turns an incapable machine into a hard
/// error instead of a silent skip.
#[allow(dead_code)]
pub fn require_vm() -> bool {
    matches!(std::env::var("VOID_BOX_REQUIRE_VM").as_deref(), Ok("1"))
}

/// Full capability check for a VM test: the kernel and initramfs artifacts
/// exist, and the platform VM backend is usable (KVM + vsock on Linux, VZ on
/// macOS). `Ok(())` means this machine can run VM tests.
#[allow(dead_code)]
pub fn detect_capability(kernel: &Path, initramfs: Option<&Path>) -> Result<(), String> {
    require_kernel_artifacts(kernel, initramfs)?;
    require_kvm_usable()?;
    require_vsock_usable()?;
    Ok(())
}

/// Decide capability and gate accordingly. Returns `true` when the machine can
/// run VM tests. On an incapable machine it prints the skip reason and returns
/// `false`, so the caller can `return` — unless `VOID_BOX_REQUIRE_VM=1`, in
/// which case it panics, because a runner that is meant to be capable and is
/// not is a broken runner, not a legitimate skip.
#[allow(dead_code)]
pub fn vm_capable_or_gate(kernel: &Path, initramfs: Option<&Path>) -> bool {
    match detect_capability(kernel, initramfs) {
        Ok(()) => true,
        Err(reason) => {
            if require_vm() {
                panic!("VOID_BOX_REQUIRE_VM=1 but the machine cannot run VM tests: {reason}");
            }
            eprintln!("skipping: {reason}");
            false
        }
    }
}

/// Handle a backend that failed to start or an RPC that failed on a machine
/// already confirmed capable. A capable machine that did not boot is a bug,
/// not a skip. On Linux this always panics. On macOS it panics only under
/// `VOID_BOX_REQUIRE_VM=1`: the hosted macOS runner reports VZ as available but
/// cannot boot nested VZ, so VZ stays advisory there — validate it on a real
/// Mac with `VOID_BOX_REQUIRE_VM=1`.
#[allow(dead_code)]
pub fn fail_capable_boot(context: &str, err: impl std::fmt::Display) {
    let strict = cfg!(target_os = "linux") || require_vm();
    if strict {
        panic!("{context}: VM failed on a capable machine (a real failure, not a skip): {err}");
    }
    eprintln!(
        "skipping ({context}): {err} [advisory on this host; set VOID_BOX_REQUIRE_VM=1 to enforce]"
    );
}

/// Bring up a VM backend for a test, or gate. This is the single entry point a
/// backend-level suite should use. It handles all three of the cases a caller
/// would otherwise have to wire by hand: the config is absent (kernel or
/// initramfs env not set), the machine is not capable, and a capable machine
/// fails to boot. A caller therefore cannot forget the fail-on-capable-boot
/// rule, because it lives here.
///
/// Returns `Some(backend)` when a VM booted, and `None` when the machine cannot
/// run VM tests (the caller should `return`). It panics when a capable machine
/// fails to boot — Linux always, macOS under `VOID_BOX_REQUIRE_VM=1` — and when
/// `VOID_BOX_REQUIRE_VM=1` but the machine is not configured or not capable.
#[allow(dead_code)]
pub async fn start_backend_or_gate(config: Option<BackendConfig>) -> Option<Box<dyn VmmBackend>> {
    let config = match config {
        Some(c) => c,
        None => {
            if require_vm() {
                panic!("VOID_BOX_REQUIRE_VM=1 but VOID_BOX_KERNEL / VOID_BOX_INITRAMFS are unset or their files are missing");
            }
            eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
            return None;
        }
    };

    if !vm_capable_or_gate(&config.kernel, config.initramfs.as_deref()) {
        return None;
    }

    let mut backend = create_backend();
    match backend.start(config).await {
        Ok(()) => Some(backend),
        Err(e) => {
            fail_capable_boot("backend start", e);
            None
        }
    }
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
