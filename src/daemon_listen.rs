//! Daemon listener configuration: address discovery, AF_UNIX vs TCP, and
//! bearer-token loading.
//!
//! The daemon defaults to AF_UNIX with mode `0o600` because loopback TCP is
//! not a privilege boundary between local Unix accounts on modern kernels.
//! Both the server (`voidbox serve`) and the client (`voidbox` CLI talking to
//! the daemon) consult the same path-discovery chain so a same-uid invocation
//! finds the socket without manual configuration. Centralizing the chain
//! avoids the failure mode where server and client disagree about where the
//! socket lives.
//!
//! TCP is opt-in via `tcp://host:port`; when enabled, a bearer token is
//! mandatory because the address space is shared with every other local user.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};

/// Environment variable consulted before falling back to a generated token.
pub const DAEMON_TOKEN_ENV: &str = "VOIDBOX_DAEMON_TOKEN";

/// Environment variable that points at a `0o600` file containing the token.
pub const DAEMON_TOKEN_FILE_ENV: &str = "VOIDBOX_DAEMON_TOKEN_FILE";

/// Length, in bytes, of randomly generated bearer tokens before hex encoding.
const GENERATED_TOKEN_BYTES: usize = 32;

/// Resolved listener configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddress {
    /// AF_UNIX path. Server creates the socket with mode `0o600`.
    Unix(PathBuf),
    /// TCP socket address. Bearer token authentication is required.
    Tcp(SocketAddr),
}

/// Errors from listener configuration parsing or path discovery.
#[derive(Debug, thiserror::Error)]
pub enum ListenConfigError {
    #[error("invalid listen address {input:?}: {detail}")]
    InvalidAddress { input: String, detail: String },
    #[error("TCP listen mode requires a bearer token: set {DAEMON_TOKEN_ENV}, {DAEMON_TOKEN_FILE_ENV}, or pass --token-file")]
    MissingTcpToken,
    #[error("token file {path}: {detail}")]
    TokenFileError { path: PathBuf, detail: String },
}

/// Parse a `--listen` value into a [`ListenAddress`].
///
/// Accepted shapes:
/// - `unix:///abs/path/voidbox.sock` — AF_UNIX, absolute path.
/// - `tcp://host:port` — TCP listener.
/// - `host:port` — back-compat alias for `tcp://host:port` (logged as legacy).
pub fn parse_listen_address(input: &str) -> Result<ListenAddress, ListenConfigError> {
    if let Some(rest) = input.strip_prefix("unix://") {
        if rest.is_empty() {
            return Err(ListenConfigError::InvalidAddress {
                input: input.to_string(),
                detail: "empty path after unix://".into(),
            });
        }
        return Ok(ListenAddress::Unix(PathBuf::from(rest)));
    }
    if let Some(rest) = input.strip_prefix("tcp://") {
        let addr: SocketAddr = rest.parse().map_err(|err: std::net::AddrParseError| {
            ListenConfigError::InvalidAddress {
                input: input.to_string(),
                detail: err.to_string(),
            }
        })?;
        return Ok(ListenAddress::Tcp(addr));
    }
    // Back-compat: bare `host:port` is treated as TCP.
    let addr: SocketAddr = input.parse().map_err(|err: std::net::AddrParseError| {
        ListenConfigError::InvalidAddress {
            input: input.to_string(),
            detail: err.to_string(),
        }
    })?;
    Ok(ListenAddress::Tcp(addr))
}

/// Discover the default AF_UNIX socket path.
///
/// The chain — `$XDG_RUNTIME_DIR/voidbox.sock` → `$TMPDIR/voidbox-$UID.sock`
/// → `/tmp/voidbox-$UID.sock` — is the same on the server and the client so
/// that a same-uid `voidbox` invocation auto-discovers the socket. The
/// per-uid suffix on the `$TMPDIR` and `/tmp` legs avoids cross-account
/// path collisions on shared hosts.
pub fn default_unix_socket_path() -> PathBuf {
    if let Some(path) = dir_socket("XDG_RUNTIME_DIR", "voidbox.sock") {
        return path;
    }
    let uid = current_uid();
    let per_uid = format!("voidbox-{uid}.sock");
    if let Some(path) = dir_socket("TMPDIR", &per_uid) {
        return path;
    }
    PathBuf::from("/tmp").join(per_uid)
}

fn dir_socket(env_var: &str, file_name: &str) -> Option<PathBuf> {
    let raw = std::env::var(env_var).ok()?;
    let dir = PathBuf::from(raw);
    if dir.as_os_str().is_empty() {
        return None;
    }
    if !is_writable_dir(&dir) {
        return None;
    }
    Some(dir.join(file_name))
}

fn is_writable_dir(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    // `access(2)` is the only portable way to ask the kernel; using mode bits
    // is not enough (ACLs, mount options like `noexec`/`ro`, etc. all matter).
    #[cfg(unix)]
    unsafe {
        use std::ffi::CString;
        let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        libc::access(c.as_ptr(), libc::W_OK) == 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `geteuid` is always safe; it returns the calling process uid.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// Bearer token resolution result.
pub struct ResolvedToken {
    /// The token bytes.
    pub token: SecretString,
    /// `Some(path)` if the token was generated and persisted at startup.
    pub generated_path: Option<PathBuf>,
}

/// Resolve a bearer token for TCP mode using, in order:
///
/// 1. `--token-file` if `cli_token_file` is `Some`.
/// 2. The `VOIDBOX_DAEMON_TOKEN_FILE` environment variable.
/// 3. The `VOIDBOX_DAEMON_TOKEN` environment variable.
/// 4. A freshly generated 32-byte hex token written to a `0o600` file under
///    `default_token_directory()` whose path is returned for the daemon to
///    log at INFO.
///
/// The CLI / env file paths must already be `0o600`; loose permissions
/// (group- or world-readable bits) are rejected up front so an operator
/// noticing a mistake during configuration is louder than discovering it
/// after a leak.
pub fn resolve_tcp_token(
    cli_token_file: Option<&Path>,
) -> Result<ResolvedToken, ListenConfigError> {
    if let Some(path) = cli_token_file {
        return read_token_file(path).map(|token| ResolvedToken {
            token,
            generated_path: None,
        });
    }
    if let Ok(path_value) = std::env::var(DAEMON_TOKEN_FILE_ENV) {
        let path = PathBuf::from(path_value);
        return read_token_file(&path).map(|token| ResolvedToken {
            token,
            generated_path: None,
        });
    }
    if let Ok(value) = std::env::var(DAEMON_TOKEN_ENV) {
        if !value.trim().is_empty() {
            return Ok(ResolvedToken {
                token: SecretString::from(value),
                generated_path: None,
            });
        }
    }
    let (path, token) = generate_and_persist_token()?;
    Ok(ResolvedToken {
        token,
        generated_path: Some(path),
    })
}

fn read_token_file(path: &Path) -> Result<SecretString, ListenConfigError> {
    let metadata = fs::metadata(path).map_err(|err| ListenConfigError::TokenFileError {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    require_token_file_perms(path, &metadata)?;
    let raw = fs::read_to_string(path).map_err(|err| ListenConfigError::TokenFileError {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(ListenConfigError::TokenFileError {
            path: path.to_path_buf(),
            detail: "token file is empty".into(),
        });
    }
    Ok(SecretString::from(trimmed))
}

#[cfg(unix)]
fn require_token_file_perms(path: &Path, metadata: &fs::Metadata) -> Result<(), ListenConfigError> {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(ListenConfigError::TokenFileError {
            path: path.to_path_buf(),
            detail: format!("token file mode is 0o{mode:03o}, must be 0o600"),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_token_file_perms(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> Result<(), ListenConfigError> {
    Ok(())
}

fn generate_and_persist_token() -> Result<(PathBuf, SecretString), ListenConfigError> {
    let mut bytes = [0u8; GENERATED_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| ListenConfigError::TokenFileError {
        path: PathBuf::new(),
        detail: format!("getrandom failed: {err}"),
    })?;
    let hex = hex_encode(&bytes);
    let dir = default_token_directory();
    fs::create_dir_all(&dir).map_err(|err| ListenConfigError::TokenFileError {
        path: dir.clone(),
        detail: err.to_string(),
    })?;
    let uid = current_uid();
    let path = dir.join(format!("daemon-token-{uid}"));
    write_token_file(&path, &hex)?;
    Ok((path, SecretString::from(hex)))
}

fn write_token_file(path: &Path, contents: &str) -> Result<(), ListenConfigError> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|err| ListenConfigError::TokenFileError {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
    file.write_all(contents.as_bytes())
        .map_err(|err| ListenConfigError::TokenFileError {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
    // Tighten mode in case the file pre-existed with looser bits before
    // truncation (OpenOptions::mode only applies to fresh creates).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            ListenConfigError::TokenFileError {
                path: path.to_path_buf(),
                detail: err.to_string(),
            }
        })?;
    }
    Ok(())
}

fn default_token_directory() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return p.join("voidbox");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home);
        if !p.as_os_str().is_empty() {
            return p.join(".void-box");
        }
    }
    PathBuf::from("/tmp").join(format!("voidbox-{}", current_uid()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Convenience: render an HTTP `Authorization: Bearer …` value containing the
/// secret, exposing the bytes only at this single call site.
pub fn bearer_header_value(token: &SecretString) -> String {
    format!("Bearer {}", token.expose_secret())
}

/// Parse the bearer token out of an `Authorization` header value if it
/// matches the `Bearer <token>` shape; otherwise `None`.
pub fn parse_bearer(header_value: &str) -> Option<&str> {
    let trimmed = header_value.trim();
    let rest = trimmed.strip_prefix("Bearer ")?;
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Cleanup wrapper that removes a stale unix socket file if it already
/// exists. Returns `Ok(())` on missing-file as well as successful removal so
/// the daemon can call this unconditionally before binding.
pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env mutation is process-global; serialize tests that set/unset env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn parse_listen_unix_form() {
        let parsed = parse_listen_address("unix:///tmp/voidbox.sock").unwrap();
        assert_eq!(
            parsed,
            ListenAddress::Unix(PathBuf::from("/tmp/voidbox.sock"))
        );
    }

    #[test]
    fn parse_listen_tcp_form() {
        let parsed = parse_listen_address("tcp://127.0.0.1:43100").unwrap();
        match parsed {
            ListenAddress::Tcp(addr) => assert_eq!(addr.port(), 43100),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn parse_listen_legacy_bare_addr_treated_as_tcp() {
        let parsed = parse_listen_address("127.0.0.1:43100").unwrap();
        match parsed {
            ListenAddress::Tcp(addr) => assert_eq!(addr.port(), 43100),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn parse_listen_unix_empty_rejected() {
        let err = parse_listen_address("unix://").unwrap_err();
        assert!(matches!(err, ListenConfigError::InvalidAddress { .. }));
    }

    #[test]
    fn discover_uses_xdg_runtime_dir_when_writable() {
        let tmp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        with_env(
            &[
                ("XDG_RUNTIME_DIR", Some(tmp.path().to_str().unwrap())),
                ("TMPDIR", None),
            ],
            || {
                let path = default_unix_socket_path();
                assert_eq!(path, tmp.path().join("voidbox.sock"));
            },
        );
    }

    #[test]
    fn discover_falls_through_to_tmpdir_when_xdg_missing() {
        let tmp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        with_env(
            &[
                ("XDG_RUNTIME_DIR", None),
                ("TMPDIR", Some(tmp.path().to_str().unwrap())),
            ],
            || {
                let path = default_unix_socket_path();
                let uid = current_uid();
                assert_eq!(path, tmp.path().join(format!("voidbox-{uid}.sock")));
            },
        );
    }

    #[test]
    fn discover_falls_back_to_slash_tmp() {
        with_env(&[("XDG_RUNTIME_DIR", None), ("TMPDIR", None)], || {
            let path = default_unix_socket_path();
            let uid = current_uid();
            assert_eq!(
                path,
                PathBuf::from("/tmp").join(format!("voidbox-{uid}.sock"))
            );
        });
    }

    #[test]
    fn discover_skips_unwritable_xdg_runtime_dir() {
        // Pointing at a path that does not exist is the simplest way to make
        // `is_writable_dir` return false without root.
        with_env(
            &[
                (
                    "XDG_RUNTIME_DIR",
                    Some("/nonexistent-voidbox-test-dir-xyzzy"),
                ),
                ("TMPDIR", None),
            ],
            || {
                let path = default_unix_socket_path();
                let uid = current_uid();
                assert_eq!(
                    path,
                    PathBuf::from("/tmp").join(format!("voidbox-{uid}.sock"))
                );
            },
        );
    }

    #[test]
    fn parse_bearer_extracts_token() {
        assert_eq!(parse_bearer("Bearer hunter2"), Some("hunter2"));
        assert_eq!(parse_bearer("  Bearer  hunter2  "), Some("hunter2"));
        assert_eq!(parse_bearer("Basic abcd"), None);
        assert_eq!(parse_bearer("Bearer "), None);
    }

    #[test]
    fn token_file_rejected_when_world_readable() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "hunter2").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let err = read_token_file(&path).unwrap_err();
        assert!(matches!(err, ListenConfigError::TokenFileError { .. }));
    }

    #[test]
    fn token_file_accepted_at_0o600() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "hunter2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let token = read_token_file(&path).unwrap();
        assert_eq!(token.expose_secret(), "hunter2");
    }

    #[test]
    fn resolve_tcp_token_prefers_cli_file() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "from-file").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        with_env(
            &[
                (DAEMON_TOKEN_ENV, Some("from-env")),
                (DAEMON_TOKEN_FILE_ENV, None),
            ],
            || {
                let resolved = resolve_tcp_token(Some(&path)).unwrap();
                assert_eq!(resolved.token.expose_secret(), "from-file");
                assert!(resolved.generated_path.is_none());
            },
        );
    }

    #[test]
    fn resolve_tcp_token_uses_env_var_when_no_file() {
        with_env(
            &[
                (DAEMON_TOKEN_ENV, Some("from-env")),
                (DAEMON_TOKEN_FILE_ENV, None),
            ],
            || {
                let resolved = resolve_tcp_token(None).unwrap();
                assert_eq!(resolved.token.expose_secret(), "from-env");
                assert!(resolved.generated_path.is_none());
            },
        );
    }

    #[test]
    fn resolve_tcp_token_generates_when_nothing_configured() {
        let runtime = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        with_env(
            &[
                (DAEMON_TOKEN_ENV, None),
                (DAEMON_TOKEN_FILE_ENV, None),
                ("XDG_RUNTIME_DIR", Some(runtime.path().to_str().unwrap())),
            ],
            || {
                let resolved = resolve_tcp_token(None).unwrap();
                let generated_path = resolved.generated_path.expect("token should be generated");
                assert!(generated_path.starts_with(runtime.path()));
                let on_disk = fs::read_to_string(&generated_path).unwrap();
                assert_eq!(on_disk.trim(), resolved.token.expose_secret());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let mode = fs::metadata(&generated_path).unwrap().mode() & 0o777;
                    assert_eq!(mode, 0o600);
                }
            },
        );
    }
}
