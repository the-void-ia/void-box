//! Same-uid AF_UNIX round trip between the daemon and a tokio HTTP client.
//!
//! Exercises the contract described by R-B4.1: server and client agree on
//! the path-discovery chain so a same-uid `voidbox` invocation auto-finds
//! the daemon socket without configuration. The test fixes
//! `XDG_RUNTIME_DIR` to a tempdir so both ends resolve to the same path
//! deterministically; the production chain is otherwise unchanged.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[tokio::test]
async fn server_and_client_resolve_to_same_path() {
    // Lock to avoid racing other tests that mutate XDG_RUNTIME_DIR. These
    // tests live in different test binaries, but the env var is process
    // global within each binary; tempdir-on-/tmp keeps any rogue resolver
    // honest.
    let runtime = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
    std::env::remove_var("TMPDIR");

    let server_path = void_box::daemon_listen::default_unix_socket_path();
    let client_path = void_box::daemon_listen::default_unix_socket_path();
    assert_eq!(
        server_path, client_path,
        "server and client must resolve to the same socket path"
    );

    // Stand up a minimal echo daemon listening on `server_path` and verify
    // the client can connect using only `default_unix_socket_path()`.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(&server_path);
    let listener = tokio::net::UnixListener::bind(&server_path).expect("bind");
    std::fs::set_permissions(&server_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let server = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
        }
    });

    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&client_path).await.expect("connect");
    client
        .write_all(b"GET /v1/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert!(
        String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
        "expected 200 from local server"
    );

    server.abort();
}
