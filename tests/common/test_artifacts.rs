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
//! with the host's own modules. An explicit `VOID_BOX_INITRAMFS` is trusted as
//! given — neither validated nor checked against the kernel — so pairing it
//! correctly is the operator's responsibility.
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
    "guest-agent/Cargo.toml",
    "claudio/src",
    "claudio/Cargo.toml",
    "void-message/src",
    "void-message/Cargo.toml",
    "void-mcp/src",
    "void-mcp/Cargo.toml",
    // The guest binaries build against void-box-protocol by path, so a protocol
    // change alters the packed binaries without touching a guest src/ tree.
    // Cargo.lock and the linker config likewise change what gets built.
    "void-box-protocol/src",
    "Cargo.lock",
    ".cargo/config.toml",
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

/// Unwrap a fallible VM op (a guest RPC on a booted VM, a host-side build
/// step), or panic with the full error-source chain. Capability is not probed
/// and artifacts are provisioned, so an error here is a real failure on a
/// machine asked to run VM tests — never a skip, on any platform. Reserve this
/// for ops after the test's first boot op has been gated through [`vm_start`]
/// or [`vm_start_value`] (or for ops that cannot raise a capability absence).
#[allow(dead_code)]
pub fn expect_vm<T, E: std::error::Error + 'static>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|err| {
        panic!(
            "{context}: VM operation failed on a capable machine (a real failure, not a skip): {}",
            format_error_chain(&err)
        )
    })
}

/// Format an error and its `source()` chain, one cause per line. `Display` on a
/// thiserror-derived [`void_box::Error`] shows only the top message; the
/// underlying I/O / vsock / serde context lives behind `source()`, and the
/// real-failure panics need every layer to be diagnosable. The skip and
/// `VOID_BOX_REQUIRE_VM` messages stay single-line with the top message only —
/// `HypervisorUnavailable` carries no source, so nothing is lost.
#[allow(dead_code)]
pub fn format_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str("\n    caused by: ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// The outcome of a VM backend `start()` attempt.
#[allow(dead_code)]
#[must_use = "SkipIncapable means the test must return early"]
pub enum VmStart {
    /// The backend started; the test proceeds.
    Ready,
    /// The host cannot virtualize; the caller must `return` (the test skips).
    /// Only produced when `VOID_BOX_REQUIRE_VM=1` is unset.
    SkipIncapable,
}

/// Classify a VM backend `start()` result by error *type*. A
/// [`void_box::Error::HypervisorUnavailable`] — raised only where a hypervisor
/// is genuinely absent — yields [`VmStart::SkipIncapable`] (a loud skip); every
/// other error panics, since those are real failures on a capable host.
/// `VOID_BOX_REQUIRE_VM=1` makes even a capability absence fail, so a runner
/// asserted capable cannot launder a lost hypervisor into a skip.
#[allow(dead_code)]
pub fn vm_start(result: Result<(), void_box::Error>, context: &str) -> VmStart {
    match vm_start_value(result, context) {
        Some(()) => VmStart::Ready,
        None => VmStart::SkipIncapable,
    }
}

/// [`vm_start`] for boot ops that return a value: a constructor like
/// `MicroVm::new`, or the first RPC on a lazily booted `Sandbox` / `VoidBox` /
/// `Pipeline`, where `vm_start`'s `Result<(), _>` signature would discard the
/// booted handle. Classification is identical: `Some(value)` proceeds, `None`
/// means the host cannot virtualize and the caller must `return` (the test
/// skips), and every other error panics as a real failure. Gate only the
/// *first* boot op per test with this — once a VM booted, a later error can no
/// longer be a capability absence, so subsequent ops belong on [`expect_vm`].
#[allow(dead_code)]
#[must_use = "None means the host cannot virtualize and the test must return early"]
pub fn vm_start_value<T>(result: Result<T, void_box::Error>, context: &str) -> Option<T> {
    let err = match result {
        Ok(value) => return Some(value),
        Err(err) => err,
    };
    if is_capability_absence(&err) {
        // Read the env var only on a capability absence: the real-failure path
        // must not depend on process env, and keeping `getenv` off that path
        // lets the honesty meta-tests exercise it concurrently with the test
        // that mutates `VOID_BOX_REQUIRE_VM` (`setenv` racing a `getenv` on
        // another thread is undefined behavior in glibc).
        let require_vm = std::env::var("VOID_BOX_REQUIRE_VM").as_deref() == Ok("1");
        if require_vm {
            panic!("{context}: VOID_BOX_REQUIRE_VM=1 but the host cannot virtualize: {err}");
        }
        eprintln!(
            "SKIP [{context}]: host cannot run VM tests (capability absent) — {err}. \
             Set VOID_BOX_REQUIRE_VM=1 to treat this as a failure."
        );
        return None;
    }
    panic!(
        "{context}: VM operation failed on a capable machine (a real failure, not a skip): {}",
        format_error_chain(&err)
    );
}

/// Create a backend and start it, or `None` when the host genuinely cannot
/// virtualize (the caller skips). A real boot failure on a capable host panics
/// inside [`vm_start`], and `VOID_BOX_REQUIRE_VM=1` makes even a capability
/// absence fail. Suites keep their own thin wrappers only to build the
/// `BackendConfig` and name the context.
#[allow(dead_code)]
pub async fn start_backend(
    config: void_box::backend::BackendConfig,
    context: &str,
) -> Option<Box<dyn void_box::backend::VmmBackend>> {
    let mut backend = void_box::backend::create_backend();
    match vm_start(backend.start(config).await, context) {
        VmStart::Ready => Some(backend),
        VmStart::SkipIncapable => None,
    }
}

/// Whether a `start()` error is a genuine capability absence — no hypervisor
/// available to this process — matched on the error *type*, not its message.
/// The backends raise [`void_box::Error::HypervisorUnavailable`] only where
/// that is true (KVM's `/dev/kvm` probe, on the absent-device and
/// access-denied errnos only; VZ's config validation on virt-less hardware). Any
/// other error is a different variant and fails, not skips — matching the
/// variant rather than a string is what stops a real `Error::Kvm` (e.g. an
/// aarch64 `KVM_ARM_VCPU_INIT` ENOENT) from being laundered into a skip.
#[allow(dead_code)]
pub fn is_capability_absence(err: &void_box::Error) -> bool {
    matches!(err, void_box::Error::HypervisorUnavailable(_))
}

/// Read `VOID_BOX_KERNEL` / `VOID_BOX_INITRAMFS` for the heavy suites that need a
/// non-test image (real Claude / Codex) and cannot auto-build it. Returns the
/// paths, or `None` after printing a skip reason — panicking under
/// `VOID_BOX_REQUIRE_VM=1`. These suites are an explicit opt-in: their image is
/// large, hash-pinned, and (for agent runs) needs real credentials, so requiring
/// the operator to stage it — rather than auto-provisioning — is deliberate.
#[allow(dead_code)]
pub fn env_artifacts_or_skip() -> Option<(PathBuf, PathBuf)> {
    let kernel = std::env::var_os("VOID_BOX_KERNEL").filter(|v| !v.is_empty());
    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").filter(|v| !v.is_empty());
    match (kernel, initramfs) {
        (Some(kernel), Some(initramfs)) => {
            let kernel = PathBuf::from(kernel);
            let initramfs = PathBuf::from(initramfs);
            assert!(
                kernel.is_file(),
                "VOID_BOX_KERNEL is set but not a file: {}",
                kernel.display()
            );
            assert!(
                initramfs.is_file(),
                "VOID_BOX_INITRAMFS is set but not a file: {}",
                initramfs.display()
            );
            Some((kernel, initramfs))
        }
        (None, None) => {
            let reason = "VOID_BOX_KERNEL / VOID_BOX_INITRAMFS are unset (this suite needs a staged non-test image)";
            assert!(
                std::env::var("VOID_BOX_REQUIRE_VM").as_deref() != Ok("1"),
                "VOID_BOX_REQUIRE_VM=1 but {reason}"
            );
            eprintln!("skipping: {reason}");
            None
        }
        // Exactly one set is a configuration error, not a reason to skip: it
        // usually means an operator typo, and silently skipping hides it.
        _ => panic!("set both VOID_BOX_KERNEL and VOID_BOX_INITRAMFS, or neither"),
    }
}

/// Record that a suite is skipping because it found no agent credentials —
/// panicking instead under `VOID_BOX_REQUIRE_AGENT_CREDS=1`.
///
/// The agent suites need a real Anthropic key or a discovered OAuth login on
/// top of a VM, and no credential reaches a pull-request runner, so their skip
/// cannot be folded into [`env_artifacts_or_skip`]'s artifact check or into
/// `VOID_BOX_REQUIRE_VM`: the Linux CI lane asserts VM capability and must
/// still not run these suites. A job that does stage a credential sets
/// `VOID_BOX_REQUIRE_AGENT_CREDS=1`, and a lost or mistyped secret then fails
/// that job instead of passing it green without ever calling the agent.
#[allow(dead_code)]
pub fn skip_without_agent_creds(reason: &str) {
    assert!(
        std::env::var("VOID_BOX_REQUIRE_AGENT_CREDS").as_deref() != Ok("1"),
        "VOID_BOX_REQUIRE_AGENT_CREDS=1 but {reason}"
    );
    eprintln!("skipping: {reason}");
}

fn resolve_artifacts() -> Result<(PathBuf, PathBuf), String> {
    // The kernel source decides the initramfs's module source: a pinned
    // download needs pinned modules; a host-kernel override pairs with the
    // host's modules. Resolve the kernel first so the initramfs can follow it.
    let (kernel, kernel_pinned) = match env_artifact("VOID_BOX_KERNEL")? {
        // A kernel override counts as pinned only when it *is* the downloaded
        // pinned kernel — the path `download_kernel.sh` and AGENTS.md tell you to
        // export. Then the initramfs must bundle matching pinned modules. Any
        // other override is treated as the host kernel, paired with host modules.
        Some(path) => {
            let pinned = same_path(&path, &downloaded_kernel_path());
            (path, pinned)
        }
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
    // Invalidate the stamp before touching the file so a concurrent reader on the
    // unlocked fast path cannot trust the in-place kernel while the download is in
    // flight. Also remove the file: download_kernel.sh treats an existing OUT_FILE
    // as a cache hit and would skip a clean re-download.
    let _ = fs::remove_file(stamp_path(&out));
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
    // Under the lock no other build is active, so clear temp/staging leftovers a
    // previously killed build may have orphaned before we make our own.
    sweep_orphan_temps();
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
        .arg("set -o pipefail; gzip -dc \"$1\" | cpio -t")
        .arg("_")
        .arg(cpio_gz.as_os_str())
        .current_dir(MANIFEST_DIR)
        .output()
        .map_err(|err| format!("could not inspect the built initramfs: {err}"))?;
    // A failed pipeline (e.g. `cpio` not installed) yields empty output; without
    // this check that would be misreported below as "no /bin/busybox".
    if !listing.status.success() {
        return Err(format!(
            "could not list the built initramfs {} (is `cpio` installed?): {}",
            cpio_gz.display(),
            String::from_utf8_lossy(&listing.stderr).trim()
        ));
    }
    let entries = String::from_utf8_lossy(&listing.stdout);

    if !entries.contains("bin/busybox") {
        return Err(format!(
            "the built initramfs {} has no /bin/busybox (no /bin/sh); install a \
             static busybox (busybox-static on Debian/Ubuntu, busybox on Fedora) \
             or set BUSYBOX=/path/to/busybox",
            cpio_gz.display()
        ));
    }
    // The pinned kernel needs the full vsock module chain; a host kernel may
    // build it in, so only require the modules when we bundled the pinned pair.
    // The base vsock.ko alone is not enough — the transport needs all three.
    // (virtio_mmio is not required: the pinned kernel has it built in.)
    if kernel_pinned {
        // The vsock chain is needed by every suite (the control channel); the 9p
        // chain by e2e_mount; overlay.ko by oci_integration. The pinned kernel
        // builds virtio_blk/ext4/virtio_net/virtio_mmio in, so those are not
        // modules and are not required here. `install_kernel_modules_from_deb`
        // only warns on a missing module, so this is the check that catches a
        // Launchpad tar-layout shift before it becomes a confusing guest error.
        for module in [
            "vsock.ko",
            "vmw_vsock_virtio_transport_common.ko",
            "vmw_vsock_virtio_transport.ko",
            "netfs.ko",
            "9pnet.ko",
            "9p.ko",
            "9pnet_virtio.ko",
            "overlay.ko",
        ] {
            if !entries.contains(module) {
                return Err(format!(
                    "the built initramfs {} is missing {module}; a guest mount or the \
                     control channel cannot come up (check the VOID_BOX_KMOD_VERSION download).",
                    cpio_gz.display()
                ));
            }
        }
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
        if !path.is_file() {
            return Err(format!(
                "BUSYBOX is set to {} but no file exists there",
                path.display()
            ));
        }
        if !is_static(&path) {
            return Err(format!(
                "BUSYBOX at {} is dynamically linked; its /bin/sh cannot run in \
                 the minimal guest. Point BUSYBOX at a static busybox.",
                path.display()
            ));
        }
        return Ok(path);
    }
    for candidate in ["/bin/busybox", "/usr/bin/busybox", "/sbin/busybox"] {
        let path = Path::new(candidate);
        if path.is_file() && is_static(path) {
            return Ok(path.to_path_buf());
        }
    }
    Err(
        "no static busybox found for the test initramfs; install busybox-static \
         (Debian/Ubuntu) or busybox (Fedora), or set BUSYBOX=/path/to/static-busybox"
            .to_string(),
    )
}

/// True when `path` is a static ELF (no dynamic interpreter). A dynamic busybox
/// packs a `/bin/sh` that cannot exec in the minimal guest, yet still lists in
/// the cpio, so existence alone is not enough. Uses `ldd`, reading both streams:
/// a shared-object dependency (`=>`) means dynamic; the explicit "not a dynamic
/// executable" / "statically linked" line is required as positive proof, so a
/// non-ELF, foreign-arch, or otherwise uninspectable file is not waved through
/// on empty output. If `ldd` itself is missing, assume static rather than block.
fn is_static(path: &Path) -> bool {
    match Command::new("ldd").arg(path).output() {
        Ok(output) => {
            let report = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if report.contains("=>") {
                return false;
            }
            report.contains("not a dynamic executable") || report.contains("statically linked")
        }
        Err(_) => true,
    }
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

/// True when both paths resolve to the same file on disk.
fn same_path(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Remove `<cache>.<pid>.tmp` files and `<staging>-<pid>` dirs a killed build may
/// have orphaned. Call only while holding the provisioning lock, so no live build
/// owns one.
fn sweep_orphan_temps() {
    let Ok(entries) = fs::read_dir(repo_path("target")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("void-box-test-rootfs.cpio.gz.") && name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        } else if name.starts_with("void-box-test-rootfs-staging-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
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

/// Pinned kernel version `(ver, upload)`. Honors a `KERNEL_VER` / `KERNEL_UPLOAD`
/// operator override the same way `download_kernel.sh` does, so the stamp and the
/// module pin follow the kernel actually downloaded, not the file's default.
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
        .arg("source scripts/lib/kernel_pin.sh && printf '%s\\n%s\\n' \"${KERNEL_VER:-$VOIDBOX_KERNEL_VER}\" \"${KERNEL_UPLOAD:-$VOIDBOX_KERNEL_UPLOAD}\"")
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

/// Run a provisioning script from the repo root, inheriting the environment plus
/// `extra_env`. `ARCH` is set explicitly so the produced filename matches the
/// path the provisioner expects. A `KERNEL_VER` override is folded into the cache
/// fingerprint via [`kernel_pin`]; other content-affecting passthroughs like
/// `VOID_BOX_MODULES_DIR` are not, so they take effect only on a cold build.
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
