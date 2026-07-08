//! In-process integration test for the credential-injection proxy.
//!
//! Wires a fake guest client (rustls, trusting the per-sandbox CA) → the real proxy
//! → a mock TLS upstream, all in one process with no VM. Asserts that:
//! - the host-held key is injected and the guest's placeholder never reaches the
//!   upstream,
//! - a missing/invalid per-sandbox token is rejected before any upstream call,
//! - a TLS SNI outside the sandbox's name constraints cannot complete a handshake.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::HeaderMap;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa,
    KeyPair, KeyUsagePurpose, NameConstraints, PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use void_box::credentials::ClaudeOAuthStore;
use void_box::proxy::injector::{ApiKeyScheme, StaticApiKeyInjector};
use void_box::proxy::{
    OAuthBearerInjector, ProxyCa, ProxyHandle, ProxyToken, SandboxContext, PROXY_TOKEN_HEADER,
};

const UPSTREAM_HOST: &str = "api.anthropic.com";
const REAL_KEY: &str = "sk-ant-real-host-held-secret";
const PLACEHOLDER_KEY: &str = "placeholder-not-a-real-key";

/// What the mock upstream observed on its most recent request.
#[derive(Clone)]
struct Captured {
    headers: HeaderMap,
    body: Bytes,
}
type CaptureSlot = Arc<Mutex<Option<Captured>>>;

/// Stand up a mock TLS upstream that records the request headers + body and
/// returns 200. Returns its address and the capture slot.
async fn start_mock_upstream() -> (SocketAddr, CaptureSlot) {
    let cert = rcgen::generate_simple_self_signed(vec![UPSTREAM_HOST.to_string()])
        .expect("self-signed upstream cert");
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("upstream server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    let captured: CaptureSlot = Arc::new(Mutex::new(None));

    let captured_for_task = captured.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let captured = captured_for_task.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let captured = captured.clone();
                    async move {
                        let headers = req.headers().clone();
                        let body = req
                            .into_body()
                            .collect()
                            .await
                            .map(|c| c.to_bytes())
                            .unwrap_or_default();
                        *captured.lock().unwrap() = Some(Captured { headers, body });
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(
                            "upstream-ok",
                        ))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });

    (addr, captured)
}

/// Build a proxy whose upstream client trusts the (self-signed) mock and routes
/// the upstream host to loopback.
fn proxy_for(mock_addr: SocketAddr) -> ProxyHandle {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve(UPSTREAM_HOST, mock_addr)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("upstream client");
    ProxyHandle::new(client)
}

/// Build a guest-side TLS connector that trusts `ca_pem`, advertising `alpn`
/// (empty = no ALPN).
fn guest_connector_with_alpn(ca_pem: &str, alpn: &[&[u8]]) -> TlsConnector {
    let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).expect("parse CA pem");
    let mut roots = RootCertStore::empty();
    roots.add(ca_der).expect("add CA root");
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    TlsConnector::from(Arc::new(config))
}

/// Build a guest-side TLS connector that trusts `ca_pem` (no ALPN).
fn guest_connector(ca_pem: &str) -> TlsConnector {
    guest_connector_with_alpn(ca_pem, &[])
}

/// Send one request through the proxy as the guest would. Returns the HTTP
/// status and the response body bytes.
async fn guest_request(
    connector: &TlsConnector,
    proxy_port: u16,
    token_header: Option<&str>,
    sni: &str,
    body: &[u8],
) -> std::io::Result<(StatusCode, Bytes)> {
    let tcp = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    let server_name = ServerName::try_from(sni.to_string()).expect("server name");
    let tls = connector.connect(server_name, tcp).await?;

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(std::io::Error::other)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", UPSTREAM_HOST)
        .header("x-api-key", PLACEHOLDER_KEY);
    if let Some(token) = token_header {
        builder = builder.header(PROXY_TOKEN_HEADER, token);
    }
    let req = builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .expect("build guest request");

    let resp = sender
        .send_request(req)
        .await
        .map_err(std::io::Error::other)?;
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(std::io::Error::other)?
        .to_bytes();
    Ok((status, body))
}

fn sandbox_context(upstream_port: u16) -> (SandboxContext, ProxyToken, String) {
    let token = ProxyToken::generate();
    let token_hex = token.to_hex();
    let ca = Arc::new(ProxyCa::generate(vec![UPSTREAM_HOST.to_string()]).expect("CA"));
    let injector = Arc::new(StaticApiKeyInjector::new(
        UPSTREAM_HOST,
        ApiKeyScheme::AnthropicXApiKey,
        SecretString::from(REAL_KEY),
    ));
    let ctx = SandboxContext::new(token.clone(), ca, injector, vec![UPSTREAM_HOST.to_string()])
        .with_upstream_port(upstream_port);
    (ctx, token, token_hex)
}

#[tokio::test(flavor = "multi_thread")]
async fn injects_real_key_and_hides_placeholder() {
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = sandbox_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    let connector = guest_connector(&ca_pem);
    let (status, body) = guest_request(
        &connector,
        binding.port,
        Some(&token_hex),
        UPSTREAM_HOST,
        b"",
    )
    .await
    .expect("guest request");

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Bytes::from("upstream-ok"));

    let seen = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream was called");
    // The real host-held key reached the upstream...
    assert_eq!(seen.headers.get("x-api-key").unwrap(), REAL_KEY);
    // ...and the guest's placeholder never did.
    assert_ne!(seen.headers.get("x-api-key").unwrap(), PLACEHOLDER_KEY);
    // The per-sandbox token was stripped before forwarding.
    assert!(seen.headers.get(PROXY_TOKEN_HEADER).is_none());

    proxy.unregister_sandbox(&token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_request_body_to_upstream() {
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = sandbox_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    let connector = guest_connector(&ca_pem);
    let payload = br#"{"model":"claude","messages":[]}"#;
    let (status, _) = guest_request(
        &connector,
        binding.port,
        Some(&token_hex),
        UPSTREAM_HOST,
        payload,
    )
    .await
    .expect("guest request");
    assert_eq!(status, StatusCode::OK);

    let seen = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream was called");
    // The request body was streamed through unmodified, with the injected key.
    assert_eq!(seen.body.as_ref(), payload);
    assert_eq!(seen.headers.get("x-api-key").unwrap(), REAL_KEY);

    proxy.unregister_sandbox(&token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_missing_and_wrong_token_without_calling_upstream() {
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = sandbox_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");
    let connector = guest_connector(&ca_pem);

    // Missing token.
    let (status, _) = guest_request(&connector, binding.port, None, UPSTREAM_HOST, b"")
        .await
        .expect("guest request");
    assert_eq!(status, StatusCode::PROXY_AUTHENTICATION_REQUIRED);

    // Wrong (but well-formed) token.
    let wrong = ProxyToken::generate().to_hex();
    let (status, _) = guest_request(&connector, binding.port, Some(&wrong), UPSTREAM_HOST, b"")
        .await
        .expect("guest request");
    assert_eq!(status, StatusCode::PROXY_AUTHENTICATION_REQUIRED);

    assert!(
        captured.lock().unwrap().is_none(),
        "upstream must not be called for an unauthenticated request"
    );

    proxy.unregister_sandbox(&token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_handshake_for_out_of_constraint_sni() {
    let (mock_addr, _captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = sandbox_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    // The guest trusts the per-sandbox CA, but the CA is name-constrained to
    // api.anthropic.com — it cannot mint a leaf for another host, so the TLS
    // handshake for an out-of-constraint SNI fails.
    let connector = guest_connector(&ca_pem);
    let result = guest_request(
        &connector,
        binding.port,
        Some(&token_hex),
        "evil.example.com",
        b"",
    )
    .await;
    assert!(
        result.is_err(),
        "handshake for an out-of-constraint SNI must fail"
    );

    proxy.unregister_sandbox(&token_hex).await;
}

/// Stand up a one-shot TLS server presenting `leaf_der`/`leaf_key`. Used to test
/// what a CA-trusting client accepts or rejects at the handshake.
async fn serve_leaf(
    leaf_der: CertificateDer<'static>,
    leaf_key: PrivateKeyDer<'static>,
) -> SocketAddr {
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![leaf_der], leaf_key)
            .expect("server cert");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind leaf server");
    let addr = listener.local_addr().expect("leaf server addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _ = acceptor.accept(stream).await;
            });
        }
    });
    addr
}

/// Attempt a TLS handshake to `addr` (SNI `sni`) as a client trusting `ca_pem`.
async fn try_handshake(ca_pem: &str, addr: SocketAddr, sni: &str) -> std::io::Result<()> {
    let connector = guest_connector(ca_pem);
    let tcp = TcpStream::connect(addr).await?;
    let server_name = ServerName::try_from(sni.to_string()).expect("server name");
    connector.connect(server_name, tcp).await.map(|_| ())
}

#[tokio::test(flavor = "multi_thread")]
async fn client_enforces_ca_name_constraints() {
    // R2 / V1: a compliant client trusting the per-sandbox CA must reject a leaf
    // for a host OUTSIDE the CA's name constraints, even with a valid signature —
    // otherwise the name-constrained CA is a universal MITM anchor. `ProxyCa`
    // refuses to *mint* such a leaf, so this forges one directly with rcgen to
    // exercise client-side enforcement. Note this validates the property in a
    // compliant TLS stack (rustls/webpki); the production guest client is
    // claude-code (Node/Bun via NODE_EXTRA_CA_CERTS), whose enforcement is the
    // separate real-client V1 gate (examples/specs/credential_proxy_claude.yaml).
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("ca key");
    let mut ca_params = CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "test name-constrained CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca_params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(UPSTREAM_HOST.to_string())],
        excluded_subtrees: Vec::new(),
    });
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign CA");
    let ca_pem = ca_cert.pem();

    // Mint a leaf for `host` signed by the constrained CA, otherwise fully valid
    // (matching SAN, ServerAuth EKU) so the only possible defect is the name
    // constraint.
    let mint = |host: &str| {
        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("leaf key");
        let mut params = CertificateParams::new(vec![host.to_string()]).expect("leaf params");
        params.distinguished_name.push(DnType::CommonName, host);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = params
            .signed_by(&leaf_key, &ca_cert, &ca_key)
            .expect("sign leaf");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        (leaf.der().clone(), key)
    };

    // Positive control: an in-constraint leaf is accepted, proving trust, EKU,
    // and signing are all otherwise valid.
    let (ok_der, ok_key) = mint(UPSTREAM_HOST);
    let ok_addr = serve_leaf(ok_der, ok_key).await;
    assert!(
        try_handshake(&ca_pem, ok_addr, UPSTREAM_HOST).await.is_ok(),
        "an in-constraint leaf must be accepted"
    );

    // The out-of-constraint leaf differs only in its name, so a rejection is the
    // client enforcing the CA's name constraints.
    let (evil_der, evil_key) = mint("evil.example.com");
    let evil_addr = serve_leaf(evil_der, evil_key).await;
    assert!(
        try_handshake(&ca_pem, evil_addr, "evil.example.com")
            .await
            .is_err(),
        "a leaf outside the CA's name constraints must be rejected by the client"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_cross_sandbox_token() {
    // On the shared proxy, sandbox A's token presented to sandbox B's listener
    // must be rejected — this is the load-bearing cross-sandbox control on KVM in
    // M0 (the per-sandbox network rule is not implemented yet, so a neighbour can
    // reach B's listener over the shared loopback). B's token authenticates B's
    // guest; A's does not.
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);

    let (ctx_a, _token_a, token_a_hex) = sandbox_context(mock_addr.port());
    let (ctx_b, _token_b, token_b_hex) = sandbox_context(mock_addr.port());
    let ca_b_pem = ctx_b.ca.ca_cert_pem().to_string();

    let _binding_a = proxy.register_sandbox(ctx_a).await.expect("register A");
    let binding_b = proxy.register_sandbox(ctx_b).await.expect("register B");

    // A neighbour reaching B's listener trusts B's CA (that is what completes the
    // TLS handshake); it then presents the only token it holds — its own, A's.
    let connector = guest_connector(&ca_b_pem);
    let (status, _) = guest_request(
        &connector,
        binding_b.port,
        Some(&token_a_hex),
        UPSTREAM_HOST,
        b"",
    )
    .await
    .expect("guest request");
    assert_eq!(status, StatusCode::PROXY_AUTHENTICATION_REQUIRED);
    assert!(
        captured.lock().unwrap().is_none(),
        "upstream must not be called for a cross-sandbox token"
    );

    proxy.unregister_sandbox(&token_a_hex).await;
    proxy.unregister_sandbox(&token_b_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ssrf_guard_rejects_internal_name_through_production_client() {
    use void_box::proxy::ssrf::SsrfGuardResolver;

    // Build the upstream client with the exact wiring `start_proxy` uses:
    // redirects disabled, host proxy env ignored, and every upstream name
    // resolved through the SSRF guard. This exercises the guard as it ships, not
    // the `is_internal_ip` unit alone.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(SsrfGuardResolver))
        .build()
        .expect("upstream client");

    // `localhost` resolves to a loopback address, which is in the baseline-deny
    // set, so the guard must refuse the resolution before any connection.
    let err = client
        .get("https://localhost/")
        .send()
        .await
        .expect_err("guard must refuse an internal upstream");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("SSRF guard"),
        "error must originate from the SSRF guard, got: {rendered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_oversize_header_block() {
    // The proxy caps its per-connection read buffer at 64 KiB (R10: strict size
    // limits on the guest-controlled parser surface). A header block larger than
    // that cannot be parsed, so the request does not complete and the upstream is
    // never reached.
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = sandbox_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    let connector = guest_connector(&ca_pem);
    let tcp = TcpStream::connect(("127.0.0.1", binding.port))
        .await
        .expect("connect proxy");
    let server_name = ServerName::try_from(UPSTREAM_HOST.to_string()).expect("server name");
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("handshake");
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .expect("http1 handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let oversize = "a".repeat(80 * 1024);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", UPSTREAM_HOST)
        .header("x-api-key", PLACEHOLDER_KEY)
        .header(PROXY_TOKEN_HEADER, &token_hex)
        .header("x-voidbox-oversize", oversize)
        .body(Full::new(Bytes::new()))
        .expect("build request");
    let result = sender.send_request(req).await;

    match result {
        Err(_) => {}
        Ok(resp) => assert_ne!(
            resp.status(),
            StatusCode::OK,
            "an oversize header block must not be proxied through"
        ),
    }
    assert!(
        captured.lock().unwrap().is_none(),
        "upstream must not be called for an oversize header block"
    );

    proxy.unregister_sandbox(&token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_does_not_negotiate_h2_and_serves_http1() {
    // V1: the proxy is HTTP/1.1-only on the client hop. A client that prefers
    // HTTP/2 (offers `h2` first in ALPN) must not end up believing it negotiated
    // h2, and its HTTP/1.1 request must still succeed.
    let (mock_addr, _captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = sandbox_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    // A client that PREFERS HTTP/2 (offers `h2` first in ALPN).
    let connector = guest_connector_with_alpn(&ca_pem, &[b"h2", b"http/1.1"]);

    let tcp = TcpStream::connect(("127.0.0.1", binding.port))
        .await
        .expect("connect proxy");
    let server_name = ServerName::try_from(UPSTREAM_HOST.to_string()).expect("server name");
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("handshake");

    // The proxy advertises no h2, so no ALPN protocol is agreed and the client
    // cannot speak h2.
    let negotiated = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    assert_ne!(
        negotiated.as_deref(),
        Some(b"h2".as_ref()),
        "proxy must not negotiate HTTP/2 on the client hop"
    );

    // The HTTP/1.1 request still completes end-to-end through the proxy.
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .expect("http1 handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", UPSTREAM_HOST)
        .header("x-api-key", PLACEHOLDER_KEY)
        .header(PROXY_TOKEN_HEADER, &token_hex)
        .body(Full::new(Bytes::new()))
        .expect("build request");
    let resp = sender.send_request(req).await.expect("http1 request");
    assert_eq!(resp.status(), StatusCode::OK);

    proxy.unregister_sandbox(&token_hex).await;
}

// ---------------------------------------------------------------------------
// OAuth injection (Claude personal) — the M1a path
// ---------------------------------------------------------------------------

/// Stand up a mock OAuth token endpoint that returns `access_token` (with a
/// rotated refresh token) for every refresh. Returns its URL and a hit counter.
async fn start_mock_token_endpoint(access_token: &str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind token mock");
    let addr = listener.local_addr().expect("token mock addr");
    let body = format!(
        r#"{{"access_token":"{access_token}","refresh_token":"rotated-refresh","expires_in":3600}}"#
    );
    let response = Arc::new(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            hits_task.fetch_add(1, Ordering::SeqCst);
            let response = response.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await; // drain request; contents unused
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/v1/oauth/token"), hits)
}

/// Build a sandbox context whose injector is OAuth-backed, with a store that
/// holds an already-expired access token (so the first request triggers a
/// refresh against `token_url`) and writes back under a temp dir kept alive by
/// the returned [`tempfile::TempDir`].
fn oauth_sandbox_context(
    upstream_port: u16,
    token_url: String,
) -> (SandboxContext, String, tempfile::TempDir) {
    let token = ProxyToken::generate();
    let token_hex = token.to_hex();
    let ca = Arc::new(ProxyCa::generate(vec![UPSTREAM_HOST.to_string()]).expect("CA"));

    let expired_ms = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        - 60_000;
    let creds = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"stale","refreshToken":"durable-refresh","expiresAt":{expired_ms}}}}}"#
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        ClaudeOAuthStore::from_json(
            &SecretString::from(creds),
            dir.path().join(".credentials.json"),
        )
        .expect("store")
        .with_token_endpoint(token_url)
        .with_http_client(reqwest::Client::builder().build().expect("client")),
    );
    let injector = Arc::new(OAuthBearerInjector::new(UPSTREAM_HOST, store));
    let ctx = SandboxContext::new(token, ca, injector, vec![UPSTREAM_HOST.to_string()])
        .with_upstream_port(upstream_port);
    (ctx, token_hex, dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_injects_minted_bearer_and_hides_placeholder() {
    let (mock_addr, captured) = start_mock_upstream().await;
    let (token_url, token_hits) = start_mock_token_endpoint("minted-oat-abc").await;
    let proxy = proxy_for(mock_addr);
    let (ctx, token_hex, _creds_dir) = oauth_sandbox_context(mock_addr.port(), token_url);
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    let connector = guest_connector(&ca_pem);
    let (status, body) = guest_request(
        &connector,
        binding.port,
        Some(&token_hex),
        UPSTREAM_HOST,
        b"",
    )
    .await
    .expect("guest request");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Bytes::from("upstream-ok"));

    let seen = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream was called");
    // The proxy refreshed the OAuth token and injected the minted Bearer...
    assert_eq!(
        seen.headers.get("authorization").unwrap(),
        "Bearer minted-oat-abc"
    );
    // ...the guest's placeholder x-api-key was dropped...
    assert!(seen.headers.get("x-api-key").is_none());
    // ...the placeholder never reached the upstream in any header...
    for (_, value) in seen.headers.iter() {
        assert_ne!(value.to_str().unwrap_or(""), PLACEHOLDER_KEY);
    }
    // ...the per-sandbox token was stripped before forwarding...
    assert!(seen.headers.get(PROXY_TOKEN_HEADER).is_none());
    // ...and exactly one refresh round-trip happened.
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);

    proxy.unregister_sandbox(&token_hex).await;
}
