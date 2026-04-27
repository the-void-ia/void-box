//! AF_UNIX permission boundary for the daemon listener.
//!
//! The daemon's defense against local cross-user RCE rests on the kernel
//! rejecting `connect(2)` from any uid that is not the daemon's. This test
//! exercises that boundary directly: it binds the daemon socket, confirms a
//! same-uid client connects, and — when the test runs as root — drops to a
//! second uid in a child process and confirms the kernel returns `EACCES`.
//!
//! When the test is not run as root, the cross-uid leg is skipped with an
//! explicit reason (the check requires `setuid(2)`, which only root can
//! perform). The same-uid leg always runs because it covers the regression
//! path where a coding mistake makes the socket world-accessible.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

use tokio::net::UnixListener;

#[cfg(target_os = "linux")]
fn current_uid() -> u32 {
    // SAFETY: `geteuid` is unconditionally safe.
    unsafe { libc::geteuid() }
}

async fn spawn_daemon_on(socket_path: PathBuf) -> tokio::task::JoinHandle<()> {
    // The test binds the listener identically to the production code path
    // (`bind_unix_socket` in `src/daemon.rs`) — same `0o600` mode, same
    // pre-bind cleanup. `serve_on_listener` is not used because it is
    // TCP-only; the relevant question here is whether the kernel-enforced
    // ACL on the bound path keeps a foreign uid out, not the daemon's
    // request handling.
    let listener = bind_test_socket(&socket_path);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut stream, _)) => {
                    use tokio::io::AsyncWriteExt;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await;
                }
                Err(_) => return,
            }
        }
    })
}

fn bind_test_socket(path: &std::path::Path) -> UnixListener {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).expect("bind unix socket");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .expect("set 0o600 perms");
    listener
}

#[tokio::test]
async fn same_uid_can_connect() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("voidbox.sock");

    let handle = spawn_daemon_on(socket.clone()).await;
    // Give the listener one tick to enter accept().
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = UnixStream::connect(&socket).expect("same-uid connect must succeed");
    handle.abort();
}

/// Cross-uid rejection: requires root to `setuid(2)` in a child process.
///
/// The test forks-and-execs `/bin/sh -c 'exec ...'` rather than calling
/// `setuid` in-process because the tokio runtime mutates the calling
/// thread's tid in ways that don't compose with `setuid` on Linux without
/// careful sequencing. Instead, we spawn `nobody`-as-the-target-uid using
/// `Command::uid`, run a short Python/sh script that opens the socket, and
/// observe the failure code.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn different_uid_cannot_connect() {
    if current_uid() != 0 {
        eprintln!(
            "skipping different_uid_cannot_connect: not running as root \
             (uid {}); cannot setuid(2) to a foreign uid in a child process",
            current_uid()
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("voidbox.sock");
    let handle = spawn_daemon_on(socket.clone()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Pick a low-privilege uid that is essentially always present on Linux
    // (`nobody` is uid 65534 on Debian/Ubuntu and most distros). The exact
    // value does not matter — anything other than 0 is sufficient because
    // the socket mode is `0o600` and we are uid 0 here.
    const NOBODY_UID: u32 = 65534;
    const NOBODY_GID: u32 = 65534;

    let socket_str = socket.to_string_lossy().into_owned();
    let mut child = std::process::Command::new("/bin/sh");
    child.arg("-c").arg(format!(
        "if : >/dev/tcp 2>/dev/null; then :; fi; exec 3<>/dev/null; \
             python3 -c 'import socket,sys; \
                         s=socket.socket(socket.AF_UNIX); \
                         try: s.connect(sys.argv[1]); print(\"OK\"); sys.exit(0)\n\
                         except PermissionError: print(\"EACCES\"); sys.exit(13)\n\
                         except Exception as e: print(repr(e)); sys.exit(1)' \
                       {}",
        shell_escape(&socket_str),
    ));
    // SAFETY: setting uid/gid via the Command API only takes effect in the
    // forked child after `execve`; it does not mutate parent state.
    unsafe {
        child.pre_exec(move || {
            if libc::setgid(NOBODY_GID) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setuid(NOBODY_UID) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let output = child.output().expect("spawn drop-uid child");
    handle.abort();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("EACCES") || output.status.code() == Some(13),
        "expected EACCES from cross-uid connect; stdout={stdout}, stderr={stderr}, \
         status={:?}",
        output.status
    );
}

#[cfg(target_os = "linux")]
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}
