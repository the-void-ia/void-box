//! On-demand provisioning of the test kernel and initramfs.
//!
//! An explicit `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` wins — the fast path CI
//! and dev1 use, where the artifacts are already staged. Otherwise the artifact
//! is downloaded or built into `target/` via the repo's own scripts, cached,
//! and reused across suites and runs. `scripts/download_kernel.sh` and
//! `scripts/build_test_image.sh` both pin their version through
//! `scripts/lib/kernel_pin.sh`, so the downloaded kernel and the initramfs
//! modules match by construction.
//!
//! A provisioning failure panics with an actionable message: a machine that
//! cannot supply artifacts fails loudly rather than skipping green. The build
//! runs only when a VM test asks for artifacts (under `--ignored`), so a plain
//! `cargo test` never pays for it.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::fs::{flock, FlockOperation};

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
const INITRAMFS_CACHE: &str = "target/void-box-test-rootfs.cpio.gz";
const PROVISION_LOCK: &str = "target/.void-box-provision.lock";

/// Absolute path under the crate root, regardless of the test's working dir.
fn repo_path(rel: &str) -> PathBuf {
    Path::new(MANIFEST_DIR).join(rel)
}

/// Resolve the guest kernel: `VOID_BOX_KERNEL` if set, else download and cache.
#[allow(dead_code)]
pub fn kernel() -> PathBuf {
    if let Some(path) = env_artifact("VOID_BOX_KERNEL") {
        return path;
    }
    let out = downloaded_kernel_path();
    if out.is_file() {
        return out;
    }
    let _lock = lock_provisioning();
    if out.is_file() {
        // Another test binary produced it while we waited for the lock.
        return out;
    }
    run_script("download_kernel.sh", &[], "download the guest kernel");
    assert_produced(&out, "download_kernel.sh");
    out
}

/// Resolve the test initramfs: `VOID_BOX_INITRAMFS` if set, else build and
/// cache. Set `VOID_BOX_TEST_REBUILD` to force a rebuild of a cached image.
#[allow(dead_code)]
pub fn test_initramfs() -> PathBuf {
    if let Some(path) = env_artifact("VOID_BOX_INITRAMFS") {
        return path;
    }
    let out = repo_path(INITRAMFS_CACHE);
    let force = std::env::var_os("VOID_BOX_TEST_REBUILD").is_some();
    if out.is_file() && !force {
        return out;
    }
    let _lock = lock_provisioning();
    if out.is_file() && !force {
        return out;
    }
    run_script(
        "build_test_image.sh",
        &[("OUT_CPIO", out.to_str().expect("cache path is valid UTF-8"))],
        "build the test initramfs",
    );
    assert_produced(&out, "build_test_image.sh");
    out
}

/// A set env var pointing at an existing file. A var that points nowhere is a
/// configuration error, not a reason to fall back to a build — panic.
fn env_artifact(var: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(var).filter(|value| !value.is_empty())?;
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "{var} is set to {} but no file exists there",
        path.display()
    );
    Some(path)
}

/// Exclusive advisory lock over the build-if-missing section, released when the
/// returned file drops. Cargo runs test binaries in parallel, so several suites
/// can reach a cold cache at once; the first holder builds, the rest wait and
/// then observe the populated cache.
fn lock_provisioning() -> File {
    let path = repo_path(PROVISION_LOCK);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("create target dir for the provision lock");
    }
    let file = File::create(&path).expect("create the provision lock file");
    flock(&file, FlockOperation::LockExclusive).expect("acquire the provision lock");
    file
}

/// Debian arch string matching the running host (both scripts use this form).
fn deb_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("no test kernel is pinned for arch {other}"),
    }
}

/// The path `download_kernel.sh` writes for this OS/arch. macOS/VZ needs an
/// uncompressed `vmlinux`; Linux/KVM uses a compressed `vmlinuz`.
fn downloaded_kernel_path() -> PathBuf {
    let name = if cfg!(target_os = "macos") {
        format!("vmlinux-{}", deb_arch())
    } else {
        format!("vmlinuz-{}", deb_arch())
    };
    repo_path(&format!("target/{name}"))
}

/// Run a provisioning script from the repo root, inheriting the environment
/// (so `BUSYBOX`, `VOID_BOX_KMOD_VERSION`, etc. pass through) plus `extra_env`.
fn run_script(script: &str, extra_env: &[(&str, &str)], goal: &str) {
    let script_path = repo_path(&format!("scripts/{script}"));
    let mut cmd = Command::new("bash");
    cmd.arg(&script_path).current_dir(MANIFEST_DIR);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let status = cmd
        .status()
        .unwrap_or_else(|err| panic!("could not run {script} to {goal}: {err}"));
    assert!(
        status.success(),
        "{script} failed to {goal} (exit {:?}). Re-run it directly to see the error. \
         The initramfs build needs a static busybox (Linux: `busybox-static`, pass \
         BUSYBOX=/bin/busybox on Ubuntu) and, on macOS, the musl cross-compiler \
         (`brew install filosottile/musl-cross/musl-cross`).",
        status.code()
    );
}

fn assert_produced(path: &Path, producer: &str) {
    assert!(
        path.is_file(),
        "{producer} reported success but {} is missing",
        path.display()
    );
}
