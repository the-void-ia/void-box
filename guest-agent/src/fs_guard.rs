//! Kernel-resolved path allowlist for privileged FS RPCs.
//!
//! `handle_write_file`, `handle_mkdir_p`, `chown_recursive`, and
//! `handle_read_file` run as PID 1 inside the guest and accept absolute
//! paths from the host over vsock. Without help, a uid-1000 agent that
//! plants a symlink under an allowlisted root (e.g.
//! `/home/sandbox/.claude -> /etc/cron.d/`) can deflect any of those
//! privileged ops out of the allowlist: a `WriteFileRequest { path:
//! "/home/sandbox/.claude/rooted", ... }` writes to `/etc/cron.d/rooted`
//! with PID-1 privileges, which is trivial root escalation inside the
//! guest.
//!
//! Lexical path checks (strip `.`/`..`, prefix-compare against an
//! allowlist) cannot defend against this. Symlink resolution happens in
//! the kernel only when the file op runs, *after* the check; the string
//! the check inspects and the inode the kernel writes to are not the
//! same object. There is also a TOCTOU window between any user-space
//! check and the file op, even ignoring symlinks.
//!
//! This module gates every path through `openat2(O_PATH)` with
//! `RESOLVE_IN_ROOT | RESOLVE_NO_SYMLINKS` against an `O_PATH |
//! O_DIRECTORY` fd cached for each entry in [`ALLOWED_WRITE_ROOTS`] /
//! [`ALLOWED_READ_ROOTS`]. The kernel walks the path, refuses to cross
//! any symlink, and returns an fd anchored *inside* the allowed root.
//! Callers use the resulting fd for the subsequent op (`write`, `read`,
//! `fchown`, `fchmod`, `mkdirat`); they never re-open the path by
//! string, since doing so would re-introduce the TOCTOU window.
//!
//! The cached fds are held for the lifetime of the process. Opening per
//! request would itself be a TOCTOU: an attacker who can replace the
//! root directory between resolution and use defeats the guarantee.
//! `openat2` and the resolve flags require Linux ≥ 5.6; if the probe
//! fails, init `_exit`s rather than `panic!`s so PID 1 dies cleanly
//! without unwinding through a partially-initialised init.
//!
//! [`ALLOWED_WRITE_ROOTS`]: crate::ALLOWED_WRITE_ROOTS
//! [`ALLOWED_READ_ROOTS`]: crate::ALLOWED_READ_ROOTS

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use nix::errno::Errno;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::Mode;

use crate::{kmsg, kmsg_emerg, ALLOWED_WRITE_ROOTS};

/// One allowlisted root and its cached `O_PATH` directory fd.
#[derive(Debug)]
struct RootEntry {
    /// The lexical root path (e.g., `/workspace`). Used to pick the
    /// longest-matching root and compute the relative tail.
    path: &'static str,
    /// Cached `O_PATH | O_DIRECTORY` fd. Held for process lifetime.
    fd: OwnedFd,
}

static WRITE_ROOTS: OnceLock<Vec<RootEntry>> = OnceLock::new();
static READ_ROOTS: OnceLock<Vec<RootEntry>> = OnceLock::new();

/// Errors from `fs_guard` resolution. Mapped to wire-format error
/// strings by the callers in `main.rs`.
#[derive(Debug)]
pub(crate) enum FsGuardError {
    /// The requested path is not absolute.
    NotAbsolute,
    /// The requested path does not fall under any allowlisted root.
    OutsideAllowedRoots,
    /// `openat2` failed (e.g., `EXDEV` from `RESOLVE_IN_ROOT`,
    /// `ELOOP` from `RESOLVE_NO_SYMLINKS`, `ENOENT` from a missing
    /// component, etc.). The wrapped errno is included for diagnostics.
    Resolve(Errno),
}

impl std::fmt::Display for FsGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute => write!(f, "path is not absolute"),
            Self::OutsideAllowedRoots => write!(f, "path is outside allowed roots"),
            Self::Resolve(errno) => {
                write!(f, "kernel-resolved path resolution failed: {errno}")
            }
        }
    }
}

/// Lazily open every entry in `ALLOWED_WRITE_ROOTS`, probe `openat2`
/// availability, and stash the fds in a process-lifetime static. Safe
/// to call multiple times; subsequent calls are O(1) once init has
/// succeeded.
///
/// Init is lazy rather than eager-at-`main` because OCI-rootfs mode
/// pivots into a new root *after* boot. Opening `/workspace` at boot
/// would cache an fd against the initramfs inode that `pivot_root`
/// then orphans. Lazy init defers the open to the first FS RPC, which
/// already gates on `wait_for_oci_setup_ready` — by that point the
/// visible filesystem matches the inodes the cached fds will name for
/// the rest of the process lifetime.
///
/// The read-side table is populated by [`init_read_roots`]; the two
/// tables are independent and can be initialised on different code
/// paths without any cross-coupling.
///
/// Failures are *fatal* and route through `kmsg_emerg` + `_exit(101)`.
/// Silent fallback to a weaker check would defeat the security
/// guarantee the rest of this module is built around.
pub(crate) fn init() {
    if WRITE_ROOTS.get().is_some() {
        return;
    }

    let write = match open_root_table(&ALLOWED_WRITE_ROOTS, "write") {
        Ok(v) => v,
        Err(msg) => fail_startup(&msg),
    };

    let probe_fd = write
        .first()
        .unwrap_or_else(|| fail_startup("fs_guard: no allowlisted write roots configured"));
    if let Err(e) = probe_openat2(probe_fd.fd.as_raw_fd()) {
        fail_startup(&format!(
            "fs_guard: openat2 unavailable on this kernel ({e}); guest-agent requires Linux \u{2265} 5.6"
        ));
    }

    let _ = WRITE_ROOTS.set(write);
    kmsg("fs_guard: cached root fds for write allowlist");
}

/// Open every entry in `roots`, treating it as the read allowlist.
/// Mirrors [`init`] but for the read side. Failures are fatal — same
/// rationale as [`init`].
pub(crate) fn init_read_roots(roots: &'static [&'static str]) {
    if READ_ROOTS.get().is_some() {
        return;
    }
    let read = match open_root_table(roots, "read") {
        Ok(v) => v,
        Err(msg) => fail_startup(&msg),
    };
    let _ = READ_ROOTS.set(read);
    kmsg("fs_guard: cached root fds for read allowlist");
}

fn open_root_table(roots: &'static [&'static str], label: &str) -> Result<Vec<RootEntry>, String> {
    let mut out = Vec::with_capacity(roots.len());
    for root in roots {
        // Make sure the directory exists; some allowlisted roots
        // (`/etc/voidbox`, `/workspace`) are created lazily by the
        // init paths and may not yet be present in every boot mode.
        if let Err(e) = std::fs::create_dir_all(root) {
            return Err(format!(
                "fs_guard: failed to ensure {label}-root '{root}' exists: {e}"
            ));
        }
        let c_path = CString::new(*root)
            .map_err(|_| format!("fs_guard: {label}-root '{root}' contains a NUL byte"))?;
        let raw = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!(
                "fs_guard: failed to open {label}-root '{root}' as O_PATH: {err}"
            ));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        out.push(RootEntry { path: root, fd });
    }
    Ok(out)
}

fn probe_openat2(dirfd: RawFd) -> Result<(), Errno> {
    let how = OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_DIRECTORY)
        .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_SYMLINKS);
    match openat2(dirfd, ".", how) {
        Ok(raw) => {
            // Drop the probe fd — close it via OwnedFd's drop.
            let _ = unsafe { OwnedFd::from_raw_fd(raw) };
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn fail_startup(msg: &str) -> ! {
    kmsg_emerg(msg);
    eprintln!("{msg}");
    unsafe { libc::_exit(101) };
}

/// Resolve `path` against the longest-matching `ALLOWED_WRITE_ROOTS`
/// entry and return an `O_PATH` fd to the resolved file or directory.
/// The kernel walks the path with `RESOLVE_IN_ROOT | RESOLVE_NO_SYMLINKS`,
/// so any intermediate symlink (planted or otherwise) causes the
/// resolution to fail rather than escape the root.
///
/// The caller can upgrade the returned fd via `openat(fd, "", ...)`
/// or use `*at` operations directly; do **not** re-open the path by
/// string after calling this — that re-introduces the TOCTOU window
/// this module exists to close.
pub(crate) fn resolve_for_write(path: &Path) -> Result<OwnedFd, FsGuardError> {
    resolve_in_table(path, write_roots())
}

/// Resolve `path` against the `ALLOWED_READ_ROOTS` table. See
/// [`resolve_for_write`] for the resolution semantics — only the root
/// table differs.
pub(crate) fn resolve_for_read(path: &Path) -> Result<OwnedFd, FsGuardError> {
    resolve_in_table(path, read_roots())
}

/// Resolve the *parent directory* of `path` against the write roots,
/// returning the parent fd and the basename to use with `*at` calls
/// such as `openat`/`mkdirat`/`unlinkat`. The parent need not yet
/// exist; if it doesn't, callers can create it via `create_dirs_in_root`.
///
/// This split lets callers create-or-truncate the *leaf* file with
/// `O_NOFOLLOW`, which is the safe pattern: the parent walk is
/// kernel-resolved with `RESOLVE_NO_SYMLINKS`, then the leaf is opened
/// against the resolved parent fd with `O_NOFOLLOW` so a planted
/// final-component symlink doesn't redirect the write.
pub(crate) fn resolve_parent_for_write(
    path: &Path,
) -> Result<(OwnedFd, std::ffi::OsString), FsGuardError> {
    if !path.is_absolute() {
        return Err(FsGuardError::NotAbsolute);
    }
    let basename = path
        .file_name()
        .ok_or(FsGuardError::OutsideAllowedRoots)?
        .to_os_string();
    let parent = path.parent().ok_or(FsGuardError::OutsideAllowedRoots)?;
    let fd = resolve_for_write(parent)?;
    Ok((fd, basename))
}

fn resolve_in_table(path: &Path, table: &[RootEntry]) -> Result<OwnedFd, FsGuardError> {
    if !path.is_absolute() {
        return Err(FsGuardError::NotAbsolute);
    }
    let normalized = lexically_normalize(path);

    // Pick the longest-matching root so that, e.g., `/home/sandbox/...`
    // is resolved against the `/home` fd if `/home` is the only root,
    // or against `/home/sandbox` if that were a separate root in the
    // future. Today the roots are non-overlapping, but the longest-match
    // rule is the right invariant.
    let entry = table
        .iter()
        .filter(|e| {
            let root = Path::new(e.path);
            normalized == root || normalized.starts_with(root)
        })
        .max_by_key(|e| e.path.len())
        .ok_or(FsGuardError::OutsideAllowedRoots)?;

    let root = Path::new(entry.path);
    let tail = normalized
        .strip_prefix(root)
        .map_err(|_| FsGuardError::OutsideAllowedRoots)?;

    // openat2 of an empty path fails with ENOENT; for the root itself
    // we resolve "." to mean "the dirfd we already have".
    let rel: PathBuf = if tail.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        tail.to_path_buf()
    };

    let how = OpenHow::new()
        .flags(OFlag::O_PATH)
        .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_SYMLINKS);

    let raw = openat2(entry.fd.as_raw_fd(), &rel, how).map_err(FsGuardError::Resolve)?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Walk `path` component-by-component starting at the matching write
/// root, creating any missing intermediate directories with `mkdirat`
/// against the latest resolved fd. Each step uses `openat2` with
/// `RESOLVE_IN_ROOT | RESOLVE_NO_SYMLINKS` so a planted symlink at
/// any level fails the walk rather than redirecting `mkdir`. Returns
/// the fd of the final directory (caller can then `fchown`/`fchmod`).
pub(crate) fn create_dirs_in_root(path: &Path) -> Result<OwnedFd, FsGuardError> {
    if !path.is_absolute() {
        return Err(FsGuardError::NotAbsolute);
    }
    let normalized = lexically_normalize(path);
    let entry = write_roots()
        .iter()
        .filter(|e| {
            let root = Path::new(e.path);
            normalized == root || normalized.starts_with(root)
        })
        .max_by_key(|e| e.path.len())
        .ok_or(FsGuardError::OutsideAllowedRoots)?;

    let root = Path::new(entry.path);
    let tail = normalized
        .strip_prefix(root)
        .map_err(|_| FsGuardError::OutsideAllowedRoots)?;

    // Start at the root fd and walk in.
    let mut current_raw = dup_owned(&entry.fd)?;

    for component in tail.components() {
        let name = match component {
            Component::Normal(n) => n,
            // The lexical normalize step removed `.`/`..`; if we still
            // see one it's a malformed path. Refuse rather than guess.
            _ => return Err(FsGuardError::OutsideAllowedRoots),
        };
        // Try to create the directory; ignore EEXIST. mkdirat respects
        // the dirfd, so a planted symlink at this name will be created
        // *as a directory* only if the symlink target doesn't already
        // exist as a non-directory; if it does, EEXIST falls through
        // and the subsequent openat2 hits RESOLVE_NO_SYMLINKS.
        match nix::sys::stat::mkdirat(
            Some(current_raw.as_raw_fd()),
            name,
            Mode::from_bits_truncate(0o755),
        ) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => return Err(FsGuardError::Resolve(e)),
        }

        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_DIRECTORY)
            .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_SYMLINKS);
        let next = openat2(current_raw.as_raw_fd(), name, how).map_err(FsGuardError::Resolve)?;
        current_raw = unsafe { OwnedFd::from_raw_fd(next) };
    }

    Ok(current_raw)
}

/// Walk `path`'s ancestors from leaf-up to and including the matching
/// write root, yielding an fd at each level. The returned `Vec` is
/// ordered from deepest (the path itself) to the root entry. Used by
/// `chown_recursive` to apply `fchown`/`fchmod` to each level via fd
/// rather than re-walking by string.
pub(crate) fn ancestors_for_write(path: &Path) -> Result<Vec<OwnedFd>, FsGuardError> {
    if !path.is_absolute() {
        return Err(FsGuardError::NotAbsolute);
    }
    let normalized = lexically_normalize(path);
    let entry = write_roots()
        .iter()
        .filter(|e| {
            let root = Path::new(e.path);
            normalized == root || normalized.starts_with(root)
        })
        .max_by_key(|e| e.path.len())
        .ok_or(FsGuardError::OutsideAllowedRoots)?;

    let root = Path::new(entry.path);
    let tail = normalized
        .strip_prefix(root)
        .map_err(|_| FsGuardError::OutsideAllowedRoots)?;

    let how = OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_DIRECTORY)
        .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_SYMLINKS);

    // Collect resolved fds at each level by re-resolving the prefix.
    // Each fchown is independent so we don't need to chain dirfds —
    // re-resolving with openat2 keeps the kernel-resolved guarantee
    // at every level.
    let mut acc = Vec::new();
    let mut prefix = root.to_path_buf();
    for component in tail.components() {
        let name = match component {
            Component::Normal(n) => n,
            _ => return Err(FsGuardError::OutsideAllowedRoots),
        };
        prefix.push(name);
        let rel = prefix
            .strip_prefix(root)
            .map_err(|_| FsGuardError::OutsideAllowedRoots)?;
        let rel_for_syscall: PathBuf = if rel.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            rel.to_path_buf()
        };
        let raw =
            openat2(entry.fd.as_raw_fd(), &rel_for_syscall, how).map_err(FsGuardError::Resolve)?;
        acc.push(unsafe { OwnedFd::from_raw_fd(raw) });
    }
    // Include the root itself last so chown applies up through the root,
    // matching the original chown_recursive's "stop after chowning the
    // allowed root" semantics.
    acc.push(dup_owned(&entry.fd)?);
    // Reverse so the leaf comes first; the caller walks leaf-up.
    acc.reverse();
    Ok(acc)
}

fn dup_owned(fd: &OwnedFd) -> Result<OwnedFd, FsGuardError> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if raw < 0 {
        return Err(FsGuardError::Resolve(Errno::last()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn write_roots() -> &'static [RootEntry] {
    if WRITE_ROOTS.get().is_none() {
        init();
    }
    WRITE_ROOTS
        .get()
        .map(Vec::as_slice)
        .expect("WRITE_ROOTS populated by init")
}

fn read_roots() -> &'static [RootEntry] {
    READ_ROOTS
        .get()
        .map(Vec::as_slice)
        .expect("fs_guard::init_read_roots() must run before resolve_for_read")
}

/// Strip `.`/`..` from the path lexically. Used to pick the right root
/// entry; the kernel resolves the actual filesystem path independently.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
            Component::Prefix(_) => {}
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    //! Integration-flavoured tests that exercise the helper against real
    //! filesystem fixtures under `tempfile`. Each test builds an isolated
    //! tempdir that *masquerades* as the allowed root by reaching into
    //! the static `WRITE_ROOTS` / `READ_ROOTS` after `init()` would
    //! normally run. This sidesteps having to fixture `/workspace`,
    //! `/home`, `/etc/voidbox` on the test host.
    //!
    //! The single-init pattern means we share the static state across
    //! all tests — `OnceLock` ignores re-`set` calls, so tests serialise
    //! through a `Mutex` and operate on the one fixture `init` configured.
    //! This matches the prod model (init once, hold for process life).

    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static FIXTURE_LOCK: Mutex<Option<FixtureRoots>> = Mutex::new(None);

    struct FixtureRoots {
        // Hold tempdirs for the whole test process. Dropping these would
        // remove the directories the cached root fds point at.
        _write_root: tempfile::TempDir,
        _read_root: tempfile::TempDir,
        write_root: PathBuf,
        read_root: PathBuf,
    }

    fn ensure_fixture() -> std::sync::MutexGuard<'static, Option<FixtureRoots>> {
        let mut guard = FIXTURE_LOCK.lock().expect("fixture lock poisoned");
        if guard.is_some() {
            return guard;
        }

        let write_dir = tempfile::tempdir().expect("create write tempdir");
        let read_dir = tempfile::tempdir().expect("create read tempdir");
        let write_root = write_dir.path().to_path_buf();
        let read_root = read_dir.path().to_path_buf();

        let write_entry = open_one_root(&write_root);
        let read_entry = open_one_root(&read_root);

        let _ = WRITE_ROOTS.set(vec![write_entry]);
        let _ = READ_ROOTS.set(vec![read_entry]);

        *guard = Some(FixtureRoots {
            _write_root: write_dir,
            _read_root: read_dir,
            write_root,
            read_root,
        });
        guard
    }

    fn open_one_root(path: &Path) -> RootEntry {
        let c = CString::new(path.as_os_str().as_encoded_bytes()).expect("tempdir path has no NUL");
        let raw = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        assert!(raw >= 0, "open tempdir as O_PATH");
        // Leak the path string so RootEntry's `&'static str` invariant
        // holds for the duration of the test process.
        let static_path: &'static str = Box::leak(
            path.to_str()
                .expect("tempdir path is utf8")
                .to_string()
                .into_boxed_str(),
        );
        RootEntry {
            path: static_path,
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        }
    }

    fn write_root_path() -> PathBuf {
        ensure_fixture()
            .as_ref()
            .expect("fixture set")
            .write_root
            .clone()
    }

    fn read_root_path() -> PathBuf {
        ensure_fixture()
            .as_ref()
            .expect("fixture set")
            .read_root
            .clone()
    }

    // Acceptance: symlink under root pointing outside is rejected.
    #[test]
    fn rejects_symlink_escape_under_write_root() {
        let root = write_root_path();
        // Plant the canonical T-B2.1 attack: a symlink inside the root
        // pointing at a location outside any allowed root.
        let outside = tempfile::tempdir().expect("outside tempdir");
        let escape = root.join("escape");
        let _ = std::fs::remove_file(&escape);
        symlink(outside.path(), &escape).expect("plant symlink");

        let target = escape.join("rooted");
        let err = resolve_parent_for_write(&target).expect_err("must reject");
        assert!(
            matches!(err, FsGuardError::Resolve(Errno::ELOOP)),
            "expected ELOOP from RESOLVE_NO_SYMLINKS, got {err:?}"
        );
    }

    // Acceptance: canonical T-B2.1 attack — `/home/sandbox/.claude ->
    // /etc/cron.d`, write to `/home/sandbox/.claude/rooted`. We model
    // this by mapping the write root to "/home/sandbox" and planting
    // ".claude" -> outside-the-root.
    #[test]
    fn rejects_canonical_planted_claude_symlink_attack() {
        let root = write_root_path();
        let outside = tempfile::tempdir().expect("outside tempdir / cron.d analogue");
        let claude = root.join(".claude");
        let _ = std::fs::remove_file(&claude);
        let _ = std::fs::remove_dir_all(&claude);
        symlink(outside.path(), &claude).expect("plant .claude symlink");

        let target = claude.join("rooted");
        let err = resolve_parent_for_write(&target).expect_err("symlink escape rejected");
        assert!(
            matches!(err, FsGuardError::Resolve(Errno::ELOOP)),
            "canonical T-B2.1 attack must be rejected with ELOOP, got {err:?}"
        );

        // The outside directory must be untouched — no file leaked through.
        let leaked = outside.path().join("rooted");
        assert!(!leaked.exists(), "no write reached the symlink target");
    }

    // Acceptance: `..` traversal that would escape is rejected.
    #[test]
    fn rejects_dotdot_traversal_attempting_escape() {
        let root = write_root_path();
        // `<root>/sub/../../etc/x` — lexically resolves to `/etc/x`.
        std::fs::create_dir_all(root.join("sub")).expect("mkdir sub");
        let target = root.join("sub").join("..").join("..").join("etc").join("x");
        let err = resolve_parent_for_write(&target).expect_err("must reject");
        assert!(
            matches!(err, FsGuardError::OutsideAllowedRoots),
            "expected OutsideAllowedRoots, got {err:?}"
        );
    }

    // Acceptance: absolute path with no allowlisted prefix is rejected.
    #[test]
    fn rejects_absolute_path_outside_roots() {
        let err = resolve_for_write(Path::new("/etc/passwd")).expect_err("must reject");
        assert!(
            matches!(err, FsGuardError::OutsideAllowedRoots),
            "expected OutsideAllowedRoots, got {err:?}"
        );
    }

    // Acceptance: bind-mount-style symlink (a symlink at a deep level
    // pointing back outside the root). We can't create real bind mounts
    // unprivileged, but a deep symlink exercises the same code path.
    #[test]
    fn rejects_deep_symlink_traversal() {
        let root = write_root_path();
        let outside = tempfile::tempdir().expect("outside tempdir");
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).expect("mkdir deep");
        let bridge = deep.join("bridge");
        let _ = std::fs::remove_file(&bridge);
        symlink(outside.path(), &bridge).expect("plant deep symlink");

        let target = bridge.join("c").join("file");
        let err = resolve_parent_for_write(&target).expect_err("must reject");
        assert!(
            matches!(err, FsGuardError::Resolve(Errno::ELOOP)),
            "expected ELOOP, got {err:?}"
        );
    }

    // Acceptance: known-good path inside the root resolves successfully.
    #[test]
    fn allows_legitimate_write_path() {
        let root = write_root_path();
        let target = root.join(".claude").join(".credentials.json");
        let parent = target.parent().unwrap();
        std::fs::create_dir_all(parent).expect("mkdir .claude");

        let (parent_fd, basename) = resolve_parent_for_write(&target).expect("legit path resolves");
        assert_eq!(basename, std::ffi::OsString::from(".credentials.json"));

        // Demonstrate the safe write idiom: openat against the resolved
        // parent fd with O_NOFOLLOW so a final-component symlink doesn't
        // redirect the write.
        let basename_c = CString::new(basename.as_encoded_bytes()).unwrap();
        let raw = unsafe {
            libc::openat(
                parent_fd.as_raw_fd(),
                basename_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        assert!(raw >= 0, "openat creates the file");
        let mut f = unsafe { std::fs::File::from_raw_fd(raw) };
        f.write_all(b"hello").expect("write succeeds");
        drop(f);

        assert!(target.exists(), "file landed at the resolved location");
    }

    // Acceptance: a deeper write under workspace-equivalent succeeds.
    #[test]
    fn allows_workspace_style_write_path() {
        let root = write_root_path();
        let nested = root.join("project").join("src");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        let target = nested.join("main.rs");
        let (parent_fd, basename) = resolve_parent_for_write(&target).expect("legit");
        assert_eq!(basename, std::ffi::OsString::from("main.rs"));
        // Just confirm the parent fd is usable.
        let basename_c = CString::new(basename.as_encoded_bytes()).unwrap();
        let raw = unsafe {
            libc::openat(
                parent_fd.as_raw_fd(),
                basename_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644,
            )
        };
        assert!(raw >= 0);
        unsafe { libc::close(raw) };
    }

    // Read-side mirrors of the write-side negatives.
    #[test]
    fn read_rejects_symlink_escape() {
        let _g = ensure_fixture();
        let root = read_root_path();
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, b"top-secret").expect("write secret");
        let bridge = root.join("escape");
        let _ = std::fs::remove_file(&bridge);
        symlink(outside.path(), &bridge).expect("plant symlink");

        let err = resolve_for_read(&bridge.join("secret.txt")).expect_err("must reject");
        assert!(
            matches!(err, FsGuardError::Resolve(Errno::ELOOP)),
            "expected ELOOP, got {err:?}"
        );
    }

    #[test]
    fn read_rejects_dotdot_escape() {
        let _g = ensure_fixture();
        let root = read_root_path();
        std::fs::create_dir_all(root.join("sub")).expect("mkdir sub");
        let target = root.join("sub").join("..").join("..").join("etc").join("x");
        let err = resolve_for_read(&target).expect_err("must reject");
        assert!(
            matches!(err, FsGuardError::OutsideAllowedRoots),
            "expected OutsideAllowedRoots, got {err:?}"
        );
    }

    #[test]
    fn read_rejects_absolute_outside_roots() {
        let _g = ensure_fixture();
        let err = resolve_for_read(Path::new("/etc/shadow")).expect_err("must reject");
        assert!(
            matches!(err, FsGuardError::OutsideAllowedRoots),
            "expected OutsideAllowedRoots, got {err:?}"
        );
    }

    #[test]
    fn read_allows_legitimate_path() {
        let root = read_root_path();
        let target = root.join("output.json");
        std::fs::write(&target, br#"{"ok":true}"#).expect("write fixture");

        let fd = resolve_for_read(&target).expect("legit read resolves");
        // Upgrade O_PATH fd to a real read fd via openat(fd, "", ...) using
        // the empty-relative-path form is fragile (needs O_EMPTYPATH on
        // older kernels); use /proc/self/fd/<fd> instead, which is the
        // documented recipe.
        let proc_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        let bytes = std::fs::read(&proc_path).expect("read via O_PATH proxy");
        assert_eq!(bytes, br#"{"ok":true}"#);
    }

    #[test]
    fn create_dirs_in_root_walks_kernel_resolved() {
        let root = write_root_path();
        let target = root.join("alpha").join("beta").join("gamma");
        let _ = std::fs::remove_dir_all(&target);
        let fd = create_dirs_in_root(&target).expect("creates levels");
        assert!(target.exists());
        // Use the returned fd to fchown — confirms it's a real fd to
        // the leaf directory.
        let res = unsafe { libc::fchown(fd.as_raw_fd(), 0, 0) };
        // EPERM is fine (running as non-root in tests); we just want to
        // verify the syscall reaches a directory.
        assert!(res == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
    }
}
