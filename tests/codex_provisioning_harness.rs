//! R9 provisioning harness: runs a **real codex binary** against the generated
//! guest provisioning (config.toml + placeholder auth.json + CA trust) and
//! asserts, on the wire, that every pinned knob is honored by that binary.
//!
//! This is the trust gate for the codex constants in `src/proxy/provision.rs`
//! (provider-entry keys, base-URL redirect, `supports_websockets = false`, the
//! `http_headers` token carrier, `CODEX_CA_CERTIFICATE`, placeholder-JWT
//! self-refresh suppression): they were derived from the codex source at the
//! pinned version, and this harness re-verifies them against the actual binary —
//! run it on every codex version bump (`scripts/agents/manifest.toml`).
//!
//! Two legs per auth mode:
//!
//! 1. **Client behavior** — codex is pointed directly at a logging TLS mock, so
//!    every header the *client* sends is visible raw: the redirect path, the
//!    placeholder Bearer, the proxy-token header, `originator`,
//!    `chatgpt-account-id`, and the absence of any WebSocket upgrade. A local
//!    refresh-endpoint override (a hit counter) proves codex never attempts its
//!    own token refresh against the placeholder JWTs.
//! 2. **Pipeline** — codex is pointed at the *real* proxy (which re-originates
//!    to the mock), proving the proxy's auth stage accepts codex's token
//!    presentation and injects the real credential end-to-end.
//!
//! The upstream hostname here is `localhost` rather than the production
//! `api.openai.com`/`chatgpt.com`, because the harness cannot edit the host's
//! `/etc/hosts` the way guest provisioning does — the name-redirect mechanics
//! are covered by the VM e2e suite instead.
//!
//! Ignored by default; needs a codex binary for this host OS. Run via
//! `scripts/test_credential_proxy_codex_v1.sh`, or directly:
//!
//! ```bash
//! VOIDBOX_CODEX_BIN=/path/to/codex \
//!   cargo test --test codex_provisioning_harness -- --ignored --nocapture --test-threads=1
//! ```

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::header::HeaderMap;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use void_box::credentials::CodexAuthMode;
use void_box::proxy::injector::{ApiKeyScheme, StaticApiKeyInjector};
use void_box::proxy::provision::{
    build_guest_provisioning, render_codex_config_toml, GuestClient, ProxiedUpstream,
};
use void_box::proxy::{ProxyCa, ProxyHandle, ProxyToken, SandboxBinding, SandboxContext};

/// The harness upstream name (see the module doc for why not the real hosts).
const HARNESS_HOST: &str = "localhost";
const REAL_BEARER_KEY: &str = "sk-harness-real-host-held-key";

/// One request the mock observed.
#[derive(Clone)]
struct SeenRequest {
    path: String,
    headers: HeaderMap,
}
type SeenLog = Arc<Mutex<Vec<SeenRequest>>>;

/// Stand up a TLS mock for `localhost` that logs every request and answers a
/// non-retryable 400 (retry-storm-proof: codex does not retry 4xx). Returns the
/// address, the request log, and the CA-equivalent cert PEM codex must trust
/// when talking to it directly.
async fn start_logging_mock() -> (SocketAddr, SeenLog, String) {
    let cert = rcgen::generate_simple_self_signed(vec![HARNESS_HOST.to_string()])
        .expect("self-signed mock cert");
    let cert_pem = cert.cert.pem();
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("mock server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));

    let seen_task = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let seen = seen_task.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(SeenRequest {
                            path: req.uri().path().to_string(),
                            headers: req.headers().clone(),
                        });
                        let body = r#"{"error":{"message":"voidbox harness: request logged"}}"#;
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(StatusCode::BAD_REQUEST)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(body)))
                                .expect("static response"),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });

    (addr, seen, cert_pem)
}

/// Spawn a plain-HTTP hit counter standing in for the OAuth refresh endpoint
/// (`CODEX_REFRESH_TOKEN_URL_OVERRIDE`). Any hit means codex tried to refresh
/// its placeholder tokens — which the far-future `exp` must prevent.
async fn start_refresh_counter() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind refresh counter");
    let addr = listener.local_addr().expect("refresh counter addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            hits_task.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let body = r#"{"error":"harness: refresh must not happen"}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (format!("http://{addr}/oauth/token"), hits)
}

/// Build the harness's ProxiedUpstream: production shape (auth mode, base path,
/// codex client) with the upstream name swapped for `localhost`.
fn harness_upstream(mode: CodexAuthMode) -> ProxiedUpstream {
    let production = ProxiedUpstream::for_provider(&void_box::llm::LlmProvider::Codex, Some(mode))
        .expect("codex maps")
        .expect("codex served");
    ProxiedUpstream {
        host: HARNESS_HOST.to_string(),
        port: production.port,
        base_path: production.base_path,
        auth: production.auth,
        client: GuestClient::CodexCli,
    }
}

/// Write a CODEX_HOME with the generated config.toml + placeholder auth.json,
/// pointing codex at `endpoint_port`. Returns the home dir and the staged env.
fn write_codex_home(
    upstream: &ProxiedUpstream,
    binding: &SandboxBinding,
    ca_pem: &str,
) -> (tempfile::TempDir, Vec<(String, String)>) {
    let provisioning = build_guest_provisioning(upstream, binding, ca_pem, "127.0.0.1");
    let home = tempfile::tempdir().expect("codex home dir");

    let config_toml = render_codex_config_toml(upstream, binding, "");
    std::fs::write(home.path().join("config.toml"), config_toml).expect("write config.toml");

    let auth_json = provisioning
        .files
        .iter()
        .find(|(path, _)| path.ends_with("auth.json"))
        .map(|(_, contents)| contents.clone())
        .expect("provisioning stages auth.json");
    std::fs::write(home.path().join("auth.json"), auth_json).expect("write auth.json");

    (home, provisioning.env)
}

/// Run `codex exec` against the provisioned CODEX_HOME with a scrubbed env, and
/// return its combined output (for diagnostics).
async fn run_codex(
    codex_bin: &str,
    codex_home: &std::path::Path,
    ca_pem_path: &std::path::Path,
    refresh_override_url: &str,
) -> String {
    let work = tempfile::tempdir().expect("workdir");
    let path_env = std::env::var("PATH").unwrap_or_default();
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(codex_bin)
            .args(["exec", "--json", "--skip-git-repo-check", "hi"])
            .current_dir(work.path())
            // Scrubbed env: no ambient OPENAI_* key may leak into the run — the
            // point is that auth comes only from the staged placeholder files.
            .env_clear()
            .env("PATH", path_env)
            .env("HOME", work.path())
            .env("CODEX_HOME", codex_home)
            .env("CODEX_CA_CERTIFICATE", ca_pem_path)
            .env("CODEX_REFRESH_TOKEN_URL_OVERRIDE", refresh_override_url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("codex exec timed out (should fail fast on the mock 400)")
    .expect("spawn codex");
    format!(
        "exit: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

/// Assertions common to every captured inference request.
fn assert_request_invariants(seen: &SeenRequest, expected_base_path: &str) {
    assert!(
        seen.path.starts_with(expected_base_path),
        "request path '{}' must start with the provisioned base path '{expected_base_path}'",
        seen.path
    );
    // R8: no WebSocket attempt — supports_websockets=false must hold.
    assert!(
        !seen.headers.contains_key("upgrade"),
        "request must not carry an Upgrade header (WS forced off)"
    );
    let connection_upgrade = seen
        .headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .map(|options| options.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false);
    assert!(
        !connection_upgrade,
        "Connection must not request an upgrade"
    );
}

fn codex_bin_or_skip() -> Option<String> {
    match std::env::var("VOIDBOX_CODEX_BIN") {
        Ok(bin) if !bin.trim().is_empty() => Some(bin),
        _ => {
            eprintln!(
                "SKIP: VOIDBOX_CODEX_BIN not set (path to a codex binary for this host OS); \
                 use scripts/test_credential_proxy_codex_v1.sh"
            );
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs a real codex binary; set VOIDBOX_CODEX_BIN (see scripts/test_credential_proxy_codex_v1.sh)"]
async fn api_key_mode_client_behavior_and_pipeline() {
    let Some(codex_bin) = codex_bin_or_skip() else {
        return;
    };

    // --- Leg 1: client behavior against the logging mock (raw headers visible).
    let (mock_addr, seen, mock_pem) = start_logging_mock().await;
    let (refresh_url, refresh_hits) = start_refresh_counter().await;

    let upstream = harness_upstream(CodexAuthMode::ApiKey);
    let binding = SandboxBinding {
        port: mock_addr.port(),
        token_hex: ProxyToken::generate().to_hex(),
    };
    let (codex_home, _env) = write_codex_home(&upstream, &binding, &mock_pem);
    let ca_path = codex_home.path().join("harness-ca.pem");
    std::fs::write(&ca_path, &mock_pem).expect("write harness CA");

    let output = run_codex(&codex_bin, codex_home.path(), &ca_path, &refresh_url).await;
    let requests = seen.lock().unwrap().clone();
    assert!(
        !requests.is_empty(),
        "codex made no request to the provisioned base URL — redirect not honored?\n{output}"
    );
    assert!(
        requests.len() <= 4,
        "unexpected request volume ({}) — retry storm?\n{output}",
        requests.len()
    );
    for request in &requests {
        assert_request_invariants(request, "/v1");
        // The placeholder key (the token carrier) is presented as the Bearer.
        assert_eq!(
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer voidbox-proxy-{}", binding.token_hex).as_str()),
            "codex must present the token-bearing placeholder as its Bearer\n{output}"
        );
        // The http_headers carrier from the generated provider entry.
        assert_eq!(
            request
                .headers
                .get("x-voidbox-proxy-token")
                .and_then(|value| value.to_str().ok()),
            Some(binding.token_hex.as_str()),
            "the provider entry's http_headers token must reach the wire\n{output}"
        );
    }
    assert_eq!(
        refresh_hits.load(Ordering::SeqCst),
        0,
        "codex must not attempt a token refresh with placeholder credentials\n{output}"
    );

    // --- Leg 2: end-to-end through the real proxy (injection observable at the
    // mock upstream).
    let (upstream_addr, upstream_seen, _upstream_pem) = start_logging_mock().await;
    let proxy_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve(HARNESS_HOST, upstream_addr)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("proxy upstream client");
    let proxy = ProxyHandle::new(proxy_client).with_loopback_bind();

    let token = ProxyToken::generate();
    let ca = Arc::new(ProxyCa::generate(vec![HARNESS_HOST.to_string()]).expect("CA"));
    let ca_pem = ca.ca_cert_pem().to_string();
    let injector = Arc::new(StaticApiKeyInjector::new(
        HARNESS_HOST,
        ApiKeyScheme::Bearer,
        SecretString::from(REAL_BEARER_KEY),
    ));
    let ctx = SandboxContext::new(token, ca, injector, vec![HARNESS_HOST.to_string()])
        .with_upstream_port(upstream_addr.port());
    let proxy_binding = proxy.register_sandbox(ctx).await.expect("register");

    let (codex_home, _env) = write_codex_home(&upstream, &proxy_binding, &ca_pem);
    let ca_path = codex_home.path().join("proxy-ca.pem");
    std::fs::write(&ca_path, &ca_pem).expect("write proxy CA");

    let output = run_codex(&codex_bin, codex_home.path(), &ca_path, &refresh_url).await;
    let requests = upstream_seen.lock().unwrap().clone();
    assert!(
        !requests.is_empty(),
        "no request reached the upstream through the proxy — token auth failed?\n{output}"
    );
    for request in &requests {
        assert_request_invariants(request, "/v1");
        // The proxy injected the real key; the placeholder and token are gone.
        assert_eq!(
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(format!("Bearer {REAL_BEARER_KEY}").as_str()),
            "the real host-held key must be injected upstream\n{output}"
        );
        assert!(
            !request.headers.contains_key("x-voidbox-proxy-token"),
            "the proxy token must be stripped before the upstream\n{output}"
        );
    }

    proxy.unregister_sandbox(&proxy_binding.token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs a real codex binary; set VOIDBOX_CODEX_BIN (see scripts/test_credential_proxy_codex_v1.sh)"]
async fn chatgpt_mode_client_behavior() {
    let Some(codex_bin) = codex_bin_or_skip() else {
        return;
    };

    let (mock_addr, seen, mock_pem) = start_logging_mock().await;
    let (refresh_url, refresh_hits) = start_refresh_counter().await;

    let upstream = harness_upstream(CodexAuthMode::ChatGpt);
    let binding = SandboxBinding {
        port: mock_addr.port(),
        token_hex: ProxyToken::generate().to_hex(),
    };
    let (codex_home, _env) = write_codex_home(&upstream, &binding, &mock_pem);
    let ca_path = codex_home.path().join("harness-ca.pem");
    std::fs::write(&ca_path, &mock_pem).expect("write harness CA");

    let output = run_codex(&codex_bin, codex_home.path(), &ca_path, &refresh_url).await;
    let requests = seen.lock().unwrap().clone();
    assert!(
        !requests.is_empty(),
        "codex made no request to the provisioned base URL — redirect not honored?\n{output}"
    );
    assert!(
        requests.len() <= 4,
        "unexpected request volume ({}) — retry storm?\n{output}",
        requests.len()
    );
    for request in &requests {
        assert_request_invariants(request, "/backend-api/codex");
        // ChatGPT mode: the Bearer is the placeholder JWT from auth.json...
        let bearer = request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(
            bearer.starts_with("Bearer ") && bearer.split('.').count() == 3,
            "codex must present the placeholder JWT as its Bearer, got '{bearer}'\n{output}"
        );
        // ...the account placeholder rides its own header (replaced by the
        // proxy's injector in production)...
        assert_eq!(
            request
                .headers
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok()),
            Some("voidbox-proxy-placeholder"),
            "codex must send the placeholder account id\n{output}"
        );
        // ...codex identifies itself...
        assert!(
            request.headers.contains_key("originator"),
            "codex must send its originator header\n{output}"
        );
        // ...and the http_headers token carrier is on the wire — in ChatGPT mode
        // it is the ONLY carrier (the Bearer is a JWT, not a token), so this
        // assertion is what the whole mode's proxy auth depends on.
        assert_eq!(
            request
                .headers
                .get("x-voidbox-proxy-token")
                .and_then(|value| value.to_str().ok()),
            Some(binding.token_hex.as_str()),
            "the provider entry's http_headers token must reach the wire\n{output}"
        );
    }
    assert_eq!(
        refresh_hits.load(Ordering::SeqCst),
        0,
        "codex must not attempt a token refresh with placeholder credentials\n{output}"
    );
}
