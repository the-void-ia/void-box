#[cfg(target_os = "linux")]
use std::fs::File;
use std::path::{Path, PathBuf};

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

// On macOS, VZ provides vsock internally. Capability is confirmed by
// `require_vz_usable` (through `require_kvm_usable`), so there is no separate
// host device to probe, and calling `require_vz_usable` again here would only
// duplicate the `sw_vers` probe.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub fn require_vsock_usable() -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(dead_code)]
pub fn require_vsock_usable() -> Result<(), String> {
    Err("no supported vsock-capable VM backend on this platform".to_string())
}

/// True when `VOID_BOX_REQUIRE_VM=1`: an incapable machine must fail, not skip.
#[allow(dead_code)]
pub fn require_vm() -> bool {
    matches!(std::env::var("VOID_BOX_REQUIRE_VM").as_deref(), Ok("1"))
}

/// Capability check: kernel/initramfs artifacts exist and the VM backend is
/// usable (KVM + `/dev/vhost-vsock` on Linux, VZ on macOS).
#[allow(dead_code)]
pub fn detect_capability(kernel: &Path, initramfs: Option<&Path>) -> Result<(), String> {
    detect_capability_inner(kernel, initramfs, true)
}

fn detect_capability_inner(
    kernel: &Path,
    initramfs: Option<&Path>,
    require_vhost_vsock: bool,
) -> Result<(), String> {
    require_kernel_artifacts(kernel, initramfs)?;
    require_kvm_usable()?;
    if require_vhost_vsock {
        require_vsock_usable()?;
    }
    Ok(())
}

/// Gate an incapable or unconfigured machine: panic under
/// `VOID_BOX_REQUIRE_VM=1`, otherwise print the reason. The caller returns after
/// calling this.
#[allow(dead_code)]
pub fn skip_or_require(reason: &str) {
    if require_vm() {
        panic!("VOID_BOX_REQUIRE_VM=1 but this machine cannot run VM tests: {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// `true` when the machine can run VM tests; otherwise gates via
/// [`skip_or_require`] and returns `false` so the caller can `return`.
#[allow(dead_code)]
pub fn vm_capable_or_gate(kernel: &Path, initramfs: Option<&Path>) -> bool {
    match detect_capability(kernel, initramfs) {
        Ok(()) => true,
        Err(reason) => {
            skip_or_require(&reason);
            false
        }
    }
}

/// Like `vm_capable_or_gate`, but does not require `/dev/vhost-vsock`. For
/// suites that force the userspace vsock backend (the snapshot tests), which do
/// not use the host vhost-vsock device.
#[allow(dead_code)]
pub fn vm_capable_or_gate_userspace_vsock(kernel: &Path, initramfs: Option<&Path>) -> bool {
    match detect_capability_inner(kernel, initramfs, false) {
        Ok(()) => true,
        Err(reason) => {
            skip_or_require(&reason);
            false
        }
    }
}

/// Read `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` and gate on capability. Returns
/// the paths when the machine can run VM tests; prints a skip reason and returns
/// `None` otherwise (panicking under `VOID_BOX_REQUIRE_VM=1`). Use at the top of
/// a `VoidBox`/`Sandbox`-level suite that builds its own config.
#[allow(dead_code)]
pub fn artifacts_or_gate() -> Option<(PathBuf, Option<PathBuf>)> {
    let kernel = std::env::var_os("VOID_BOX_KERNEL")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let Some(kernel) = kernel else {
        skip_or_require("VOID_BOX_KERNEL is unset");
        return None;
    };
    if !vm_capable_or_gate(&kernel, initramfs.as_deref()) {
        return None;
    }
    Some((kernel, initramfs))
}

/// A VM op that failed on a capable machine is a bug, not a skip. Panics on
/// Linux always; on macOS only under `VOID_BOX_REQUIRE_VM=1`, since the hosted
/// macOS runner reports VZ available but cannot boot nested VZ (advisory there).
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

/// Wrap a fallible VM op (start, `VoidBox::run`, an RPC): `Ok` returns the
/// value; `Err` goes through [`fail_capable_boot`]. Call only after capability
/// is confirmed.
#[allow(dead_code)]
pub fn checked_vm<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            fail_capable_boot(context, e);
            None
        }
    }
}

/// Bring up a VM backend, or gate. Skips when the config is absent or the
/// machine is incapable; fails a capable machine's boot via [`checked_vm`].
/// Returns `None` (caller returns) on a skip; panics on a capable-boot failure
/// or under `VOID_BOX_REQUIRE_VM=1`.
#[allow(dead_code)]
pub async fn start_backend_or_gate(config: Option<BackendConfig>) -> Option<Box<dyn VmmBackend>> {
    let config = match config {
        Some(c) => c,
        None => {
            skip_or_require("VOID_BOX_KERNEL / VOID_BOX_INITRAMFS are unset");
            return None;
        }
    };

    if !vm_capable_or_gate(&config.kernel, config.initramfs.as_deref()) {
        return None;
    }

    let mut backend = create_backend();
    checked_vm(backend.start(config).await, "backend start")?;
    Some(backend)
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

    Ok(())
}
