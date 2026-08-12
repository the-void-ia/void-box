//! On-demand provisioning of the guest kernel + test initramfs for VM tests.
//!
//! An explicit `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` wins — the fast path CI
//! and a pre-staged local checkout use. Otherwise the kernel and initramfs are
//! provisioned as a matched pair into `target/` via the repo's own scripts,
//! validated, cached behind a per-checkout `flock`, and reused. The pairing is
//! deliberate: `download_kernel.sh` fetches the kernel pinned by
//! `kernel_pin.sh`, and when the initramfs is built for that pinned kernel the
//! provisioner passes the same pin as `VOID_BOX_KMOD_VERSION`, so the bundled
//! modules match the kernel's vermagic. A host-kernel override instead pairs
//! with the host's own modules.
//!
//! A provisioning failure panics with an actionable message: a machine that
//! cannot supply artifacts fails loudly rather than skipping green. Reuse is
//! gated on a `.stamp` fingerprint, not mere file existence, so a corrupt,
//! partial, or stale cache is rebuilt rather than trusted. Provisioning is
//! lazy — only a VM test (under `--ignored`) calls [`artifacts`], so a plain
//! `cargo test` never pays for it.
//!
//! Note: the build shells out to `build_test_image.sh`, which runs `cargo
//! build`; that nested build takes cargo's build-directory lock only after the
//! parent test binary has released it.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::SystemTime;

use rustix::fs::{flock, FlockOperation};

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
const INITRAMFS_CACHE: &str = "target/void-box-test-rootfs.cpio.gz";
const INITRAMFS_STAGING: &str = "target/void-box-test-rootfs-staging";
const PROVISION_LOCK: &str = "target/.void-box-provision.lock";

/// Guest source trees and build inputs whose changes must invalidate the cached
/// initramfs. A stale image would run yesterday's guest code and still pass.
const INITRAMFS_INPUTS: &[&str] = &[
    "guest-agent/src",
    "claudio/src",
    "void-message/src",
    "void-mcp/src",
    "scripts/build_test_image.sh",
    "scripts/lib",
];

/// Minimum plausible kernel size; a smaller file is a truncated download.
const MIN_KERNEL_BYTES: u64 = 1_000_000;

/// Resolve once per test-binary process. Memoizing the failure too means a
/// broken toolchain fails fast instead of rebuilding for every test.
static ARTIFACTS: OnceLock<Result<(PathBuf, PathBuf), String>> = OnceLock::new();
static KERNEL_PIN: OnceLock<Result<(String, String), String>> = OnceLock::new();

/// The guest kernel and test initramfs, provisioning them if needed. Panics
/// with an actionable message on any provisioning failure.
#[allow(dead_code)]
pub fn artifacts() -> (PathBuf, PathBuf) {
    match ARTIFACTS.get_or_init(resolve_artifacts) {
        Ok(pair) => pair.clone(),
        Err(err) => panic!("{err}"),
    }
}

fn resolve_artifacts() -> Result<(PathBuf, PathBuf), String> {
    // The kernel source decides the initramfs's module source: a pinned
    // download needs pinned modules; a host-kernel override pairs with the
    // host's modules. Resolve the kernel first so the initramfs can follow it.
    let (kernel, kernel_pinned) = match env_artifact("VOID_BOX_KERNEL")? {
        Some(path) => (path, false),
        None => (provision_kernel()?, true),
    };

    let initramfs = match env_artifact("VOID_BOX_INITRAMFS")? {
        Some(path) => path,
        None => provision_initramfs(kernel_pinned)?,
    };

    Ok((kernel, initramfs))
}

/// A set env var pointing at an existing file. Set-but-missing is a
/// configuration error, not a reason to fall back to a build — surface it.
fn env_artifact(var: &str) -> Result<Option<PathBuf>, String> {
    let Some(raw) = std::env::var_os(var).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(raw);
    if !path.is_file() {
        return Err(format!(
            "{var} is set to {} but no file exists there",
            path.display()
        ));
    }
    Ok(Some(path))
}

// ---------------------------------------------------------------------------
// Kernel
// ---------------------------------------------------------------------------

fn provision_kernel() -> Result<PathBuf, String> {
    let out = downloaded_kernel_path();
    let key = format!("kernel:{}:{}", arch(), pin_tuple()?);
    if cache_valid(&out, &key) {
        return Ok(out);
    }
    let _provision_lock = lock_provisioning()?;
    if cache_valid(&out, &key) {
        return Ok(out); // produced while we waited for the lock
    }
    // download_kernel.sh treats an existing OUT_FILE as a cache hit, so remove a
    // possibly partial or stale kernel first to force a clean re-download.
    let _ = fs::remove_file(&out);
    run_script(
        "download_kernel.sh",
        &[("ARCH", OsStr::new(arch()))],
        "download the guest kernel",
    )?;
    let len = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if len < MIN_KERNEL_BYTES {
        return Err(format!(
            "download_kernel.sh reported success but {} is {len} bytes (truncated?)",
            out.display()
        ));
    }
    write_stamp(&out, &key)?;
    Ok(out)
}

/// The path `download_kernel.sh` writes for this OS/arch. macOS/VZ needs an
/// uncompressed `vmlinux`; Linux/KVM uses a compressed `vmlinuz`.
fn downloaded_kernel_path() -> PathBuf {
    let stem = if cfg!(target_os = "macos") {
        "vmlinux"
    } else {
        "vmlinuz"
    };
    repo_path(&format!("target/{stem}-{}", deb_arch()))
}

// ---------------------------------------------------------------------------
// Initramfs
// ---------------------------------------------------------------------------

fn provision_initramfs(kernel_pinned: bool) -> Result<PathBuf, String> {
    let out = repo_path(INITRAMFS_CACHE);
    let key = initramfs_fingerprint(kernel_pinned)?;
    let force = std::env::var_os("VOID_BOX_TEST_REBUILD").is_some();
    if !force && cache_valid(&out, &key) {
        return Ok(out);
    }
    let _provision_lock = lock_provisioning()?;
    if !force && cache_valid(&out, &key) {
        return Ok(out);
    }
    build_initramfs(&out, kernel_pinned)?;
    write_stamp(&out, &key)?;
    Ok(out)
}

fn build_initramfs(final_path: &Path, kernel_pinned: bool) -> Result<(), String> {
    // Build into per-process temp paths, then rename on success, so an
    // interrupted build never leaves a truncated cache file for a later run to
    // reuse. OUT_DIR is under target/ (build_test_image.sh `rm -rf`s it).
    let pid = std::process::id();
    let tmp_cpio = repo_path(&format!("{INITRAMFS_CACHE}.{pid}.tmp"));
    let staging = repo_path(&format!("{INITRAMFS_STAGING}-{pid}"));

    let ver;
    let upload;
    let busybox;
    let mut env: Vec<(&str, &OsStr)> = vec![
        ("OUT_CPIO", tmp_cpio.as_os_str()),
        ("OUT_DIR", staging.as_os_str()),
        ("ARCH", OsStr::new(arch())),
    ];
    // Linux: install_busybox only warns (exit 0) when BUSYBOX is unset, packing
    // a shell-less image; resolve a real one so the build cannot soft-fail.
    // macOS handles busybox itself (ensure_busybox_macos).
    if cfg!(target_os = "linux") {
        busybox = resolve_busybox()?;
        env.push(("BUSYBOX", busybox.as_os_str()));
    }
    // Pin the initramfs modules to the downloaded kernel so vermagic matches.
    if kernel_pinned {
        let pin = kernel_pin()?;
        ver = pin.0;
        upload = pin.1;
        env.push(("VOID_BOX_KMOD_VERSION", OsStr::new(&ver)));
        env.push(("VOID_BOX_KMOD_UPLOAD", OsStr::new(&upload)));
    }

    let result = (|| {
        run_script("build_test_image.sh", &env, "build the test initramfs")?;
        validate_initramfs(&tmp_cpio, kernel_pinned)?;
        fs::rename(&tmp_cpio, final_path)
            .map_err(|err| format!("could not finalize initramfs cache: {err}"))
    })();

    let _ = fs::remove_file(&tmp_cpio);
    let _ = fs::remove_dir_all(&staging);
    result
}

/// A script that exits 0 is not proof of a usable image: `install_busybox` and
/// the module installers warn-and-continue. Inspect the packed cpio and require
/// the entries the guest cannot boot or exec without.
fn validate_initramfs(cpio_gz: &Path, kernel_pinned: bool) -> Result<(), String> {
    let listing = Command::new("bash")
        .arg("-c")
        .arg("gzip -dc \"$1\" | cpio -t 2>/dev/null")
        .arg("_")
        .arg(cpio_gz.as_os_str())
        .current_dir(MANIFEST_DIR)
        .output()
        .map_err(|err| format!("could not inspect the built initramfs: {err}"))?;
    let entries = String::from_utf8_lossy(&listing.stdout);

    if !entries.contains("bin/busybox") {
        return Err(format!(
            "the built initramfs {} has no /bin/busybox (no /bin/sh); install a \
             static busybox (busybox-static on Debian/Ubuntu, busybox on Fedora) \
             or set BUSYBOX=/path/to/busybox",
            cpio_gz.display()
        ));
    }
    // The pinned kernel needs vsock as a module; a host kernel may build it in,
    // so only require the module when we bundled the pinned pair.
    if kernel_pinned && !entries.contains("vsock.ko") {
        return Err(format!(
            "the built initramfs {} has no vsock.ko; the guest control channel \
             cannot come up. Check VOID_BOX_KMOD_VERSION module download.",
            cpio_gz.display()
        ));
    }
    Ok(())
}

/// Fingerprint that changes whenever a rebuild is required: the arch, the module
/// source (pinned tuple or host), and the newest mtime across guest sources.
fn initramfs_fingerprint(kernel_pinned: bool) -> Result<String, String> {
    let module_source = if kernel_pinned {
        format!("pinned-{}", pin_tuple()?)
    } else {
        "host".to_string()
    };
    let newest = INITRAMFS_INPUTS
        .iter()
        .map(|rel| newest_mtime(&repo_path(rel)))
        .max()
        .unwrap_or(0);
    Ok(format!("initramfs:{}:{module_source}:{newest}", arch()))
}

/// Newest modification time (unix nanos) at or below `path`, or 0 if absent.
fn newest_mtime(path: &Path) -> u128 {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return 0,
    };
    let mut newest = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|dur| dur.as_nanos())
        .unwrap_or(0);
    if meta.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                newest = newest.max(newest_mtime(&entry.path()));
            }
        }
    }
    newest
}

/// Prefer a busybox at a static-package location. On Debian/Ubuntu the dynamic
/// `busybox` package shadows the static one in PATH, and a dynamic busybox packs
/// a `/bin/sh` that cannot run in the minimal guest.
fn resolve_busybox() -> Result<PathBuf, String> {
    if let Some(raw) = std::env::var_os("BUSYBOX").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "BUSYBOX is set to {} but no file exists there",
            path.display()
        ));
    }
    for candidate in ["/bin/busybox", "/usr/bin/busybox", "/sbin/busybox"] {
        if Path::new(candidate).is_file() {
            return Ok(PathBuf::from(candidate));
        }
    }
    Err(
        "no busybox found for the test initramfs; install busybox-static \
         (Debian/Ubuntu) or busybox (Fedora), or set BUSYBOX=/path/to/busybox"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Cache stamps and the provisioning lock
// ---------------------------------------------------------------------------

/// True when `artifact` exists and its `.stamp` records exactly `key`. A stamp
/// is written only after a build succeeds and validates, so its presence means
/// the artifact is complete and current.
fn cache_valid(artifact: &Path, key: &str) -> bool {
    artifact.is_file() && fs::read_to_string(stamp_path(artifact)).is_ok_and(|s| s == key)
}

fn write_stamp(artifact: &Path, key: &str) -> Result<(), String> {
    fs::write(stamp_path(artifact), key)
        .map_err(|err| format!("could not write cache stamp: {err}"))
}

fn stamp_path(artifact: &Path) -> PathBuf {
    let mut raw = artifact.as_os_str().to_owned();
    raw.push(".stamp");
    PathBuf::from(raw)
}

/// Exclusive advisory lock over the build-if-missing section, released when the
/// returned file drops. Cargo runs test binaries in parallel, so several suites
/// can reach a cold cache at once; the first holder builds, the rest wait and
/// then observe the populated cache.
fn lock_provisioning() -> Result<File, String> {
    let path = repo_path(PROVISION_LOCK);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("could not create target dir: {err}"))?;
    }
    let file = File::create(&path).map_err(|err| format!("could not create lock file: {err}"))?;
    flock(&file, FlockOperation::LockExclusive)
        .map_err(|err| format!("could not acquire the provision lock: {err}"))?;
    Ok(file)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn repo_path(rel: &str) -> PathBuf {
    Path::new(MANIFEST_DIR).join(rel)
}

/// Host arch as the scripts and `std::env::consts` both spell it.
fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// Debian arch suffix `download_kernel.sh` puts in its output filename.
fn deb_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("no test kernel is pinned for arch {other}"),
    }
}

/// Pinned kernel version from `kernel_pin.sh`: `(VOIDBOX_KERNEL_VER, _UPLOAD)`.
fn kernel_pin() -> Result<(String, String), String> {
    KERNEL_PIN.get_or_init(read_kernel_pin).clone()
}

fn pin_tuple() -> Result<String, String> {
    let (ver, upload) = kernel_pin()?;
    Ok(format!("{ver}.{upload}"))
}

fn read_kernel_pin() -> Result<(String, String), String> {
    let output = Command::new("bash")
        .arg("-c")
        .arg("source scripts/lib/kernel_pin.sh && printf '%s\\n%s\\n' \"$VOIDBOX_KERNEL_VER\" \"$VOIDBOX_KERNEL_UPLOAD\"")
        .current_dir(MANIFEST_DIR)
        .output()
        .map_err(|err| format!("could not read scripts/lib/kernel_pin.sh: {err}"))?;
    if !output.status.success() {
        return Err("sourcing scripts/lib/kernel_pin.sh failed".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let ver = lines.next().unwrap_or("").trim().to_string();
    let upload = lines.next().unwrap_or("").trim().to_string();
    if ver.is_empty() || upload.is_empty() {
        return Err("kernel_pin.sh did not define VOIDBOX_KERNEL_VER / _UPLOAD".to_string());
    }
    Ok((ver, upload))
}

/// Run a provisioning script from the repo root, inheriting the environment
/// (so any operator-set `KERNEL_VER`, `VOID_BOX_MODULES_DIR`, etc. pass through)
/// plus `extra_env`. `ARCH` is always set explicitly so the produced filename
/// matches the path the provisioner expects.
fn run_script(script: &str, extra_env: &[(&str, &OsStr)], goal: &str) -> Result<(), String> {
    let script_path = repo_path(&format!("scripts/{script}"));
    let mut cmd = Command::new("bash");
    cmd.arg(&script_path).current_dir(MANIFEST_DIR);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let status = cmd
        .status()
        .map_err(|err| format!("could not run {script} to {goal}: {err}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "{script} failed to {goal} (exit {:?}). Re-run it directly to see the error. \
         The initramfs build needs a static busybox (Debian/Ubuntu: `busybox-static`) \
         and, on macOS, the musl cross-compiler \
         (`brew install filosottile/musl-cross/musl-cross`).",
        status.code()
    ))
}
