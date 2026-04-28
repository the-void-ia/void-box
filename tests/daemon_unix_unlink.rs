//! AF_UNIX socket-file lifecycle for the daemon.
//!
//! The daemon binds an AF_UNIX listener at startup and must remove the
//! socket file on every exit path so the next startup begins from a clean
//! filesystem state and operators do not see stale entries in `ls` or
//! `netstat`. The unlink runs from a `Drop` impl on a guard held by the
//! serve loop, so cancellation, panic, signal-driven shutdown, and clean
//! returns all converge on the same code.
//!
//! This test exercises the cancellation path: it spawns the daemon on a
//! tempdir socket path, waits for the file to appear, aborts the task,
//! and then confirms the file is gone. Aborting a tokio task drops every
//! local on the suspended future's stack — including the guard — so the
//! `Drop` runs synchronously as part of the abort.

use std::path::Path;
use std::time::{Duration, Instant};

use void_box::daemon::{serve, ServeConfig};
use void_box::daemon_listen::ListenAddress;

async fn wait_for(predicate: impl Fn() -> bool, timeout: Duration, label: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {label}");
}

#[tokio::test]
async fn unix_socket_unlinked_on_serve_cancel() {
    // Isolate the daemon's persistence so other test binaries do not race
    // on `VOIDBOX_STATE_DIR`. Each test gets its own tempdir; setting the
    // env before spawning the daemon is sound because we read it back
    // through `provider_from_env` inside `build_app_state`.
    let state_dir = tempfile::tempdir().expect("state tempdir");
    std::env::set_var("VOIDBOX_STATE_DIR", state_dir.path());

    let socket_dir = tempfile::tempdir().expect("socket tempdir");
    let socket_path = socket_dir.path().join("voidbox.sock");

    let path_for_task = socket_path.clone();
    let task = tokio::spawn(async move {
        let _ = serve(ServeConfig {
            address: ListenAddress::Unix(path_for_task),
            token: None,
        })
        .await;
    });

    let exists_path = socket_path.clone();
    wait_for(
        move || Path::new(&exists_path).exists(),
        Duration::from_secs(5),
        "socket file to appear",
    )
    .await;

    task.abort();
    // Yield until the task observes the abort and unwinds; awaiting the
    // join handle gives tokio a chance to drop the suspended future,
    // which in turn drops the guard and unlinks the socket file.
    let _ = task.await;

    let gone_path = socket_path.clone();
    wait_for(
        move || !Path::new(&gone_path).exists(),
        Duration::from_secs(2),
        "socket file to be removed",
    )
    .await;
}
