//! Integration coverage for the daemon's auth chokepoint.
//!
//! The daemon's hand-rolled HTTP server matches every route at one site
//! (`route_request` in `src/daemon.rs`); the auth check landed at the top
//! of that function so every route inherits it. These tests boot a real
//! TCP listener with a known bearer token and confirm:
//!
//! - `Authorization: Bearer <correct>` succeeds (here, surfaces a 4xx
//!   from the underlying handler — the auth gate was passed).
//! - Missing or wrong tokens fail with `401 Unauthorized`.
//! - Every route enumerated in R-B4.1 inherits the gate.
//!
//! The same-uid AF_UNIX path is exercised in `daemon_unix_perms.rs`.

#[path = "common/net.rs"]
mod test_net;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::OnceLock;
use std::time::Duration;

use secrecy::SecretString;

const TOKEN: &str = "deadbeef-test-token";

/// All tests in this binary share one state dir. `VOIDBOX_STATE_DIR` is
/// process-global env, and `cargo test` runs `#[test]`s concurrently —
/// the previous design's mutex-around-set_var only narrowed the race
/// window without closing it (the daemon's env read happens later, in
/// `build_app_state`, after the lock drops). Initialising via `OnceLock`
/// removes the race surface entirely: the env is set exactly once, every
/// daemon thread reads the same value. The auth tests don't write
/// conflicting state — they exercise the chokepoint by route, not the
/// run-store contents — so a shared dir is sound.
static SHARED_STATE_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

fn ensure_shared_state_dir() {
    SHARED_STATE_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("create shared state tempdir");
        // Set once across all tests. `get_or_init` serialises concurrent
        // first callers internally, so the env mutation is single-writer.
        std::env::set_var("VOIDBOX_STATE_DIR", dir.path());
        dir
    });
}

fn start_daemon_with_token() -> SocketAddr {
    ensure_shared_state_dir();
    let (addr, listener) = test_net::reserve_localhost_listener();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tokio_listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let _ = void_box::daemon::serve_on_listener_with_token(
                tokio_listener,
                SecretString::from(TOKEN.to_string()),
            )
            .await;
        });
    });

    for _ in 0..50 {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            return addr;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not start within timeout");
}

fn send(addr: SocketAddr, method: &str, path: &str, header: Option<&str>, body: &str) -> u16 {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let auth_line = header
        .map(|h| format!("Authorization: {h}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n{auth_line}\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

#[test]
fn missing_token_yields_401_on_protected_routes() {
    let addr = start_daemon_with_token();
    let body = "{}";
    let routes = [
        ("POST", "/v1/runs"),
        ("GET", "/v1/runs/some-id/telemetry"),
        ("GET", "/v1/runs/some-id/stages/build/output-file"),
        ("POST", "/v1/runs/some-id/cancel"),
        ("POST", "/v1/runs/some-id/messages"),
        ("GET", "/v1/health"),
    ];
    for (method, path) in routes {
        let status = send(addr, method, path, None, body);
        assert_eq!(
            status, 401,
            "expected 401 on {method} {path} without auth header"
        );
    }
}

#[test]
fn wrong_token_yields_401() {
    let addr = start_daemon_with_token();
    let status = send(
        addr,
        "GET",
        "/v1/health",
        Some("Bearer not-the-token"),
        "{}",
    );
    assert_eq!(status, 401);
}

#[test]
fn correct_token_passes_auth_gate() {
    let addr = start_daemon_with_token();
    let header = format!("Bearer {TOKEN}");
    // /v1/health is the only route that does not require any further state
    // to return 2xx; the point of this test is "auth gate passes", not
    // "every handler is happy with an empty body".
    let status = send(addr, "GET", "/v1/health", Some(&header), "");
    assert_eq!(status, 200, "expected 200 with valid bearer token");
}
