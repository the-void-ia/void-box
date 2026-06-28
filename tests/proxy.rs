//! In-process integration test for the credential-injection proxy.
//!
//! Wires a fake guest client (rustls, trusting the per-run CA) → the real proxy
//! → a mock TLS upstream, all in one process with no VM. Asserts that:
//! - the host-held key is injected and the guest's placeholder never reaches the
//!   upstream,
//! - a missing/invalid per-run token is rejected before any upstream call,
//! - a TLS SNI outside the run's name constraints cannot complete a handshake.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::header::HeaderMap;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use secrecy::SecretString;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use void_box::proxy::injector::{ApiKeyScheme, StaticApiKeyInjector};
use void_box::proxy::{ProxyCa, ProxyHandle, ProxyToken, RunContext, PROXY_TOKEN_HEADER};

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

/// Build a guest-side TLS connector that trusts `ca_pem`.
fn guest_connector(ca_pem: &str) -> TlsConnector {
    let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).expect("parse CA pem");
    let mut roots = RootCertStore::empty();
    roots.add(ca_der).expect("add CA root");
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
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

fn run_context(upstream_port: u16) -> (RunContext, ProxyToken, String) {
    let token = ProxyToken::generate();
    let token_hex = token.to_hex();
    let ca = Arc::new(ProxyCa::generate(vec![UPSTREAM_HOST.to_string()]).expect("CA"));
    let injector = Arc::new(StaticApiKeyInjector::new(
        UPSTREAM_HOST,
        ApiKeyScheme::AnthropicXApiKey,
        SecretString::from(REAL_KEY),
    ));
    let ctx = RunContext::new(token.clone(), ca, injector, vec![UPSTREAM_HOST.to_string()])
        .with_upstream_port(upstream_port);
    (ctx, token, token_hex)
}

#[tokio::test(flavor = "multi_thread")]
async fn injects_real_key_and_hides_placeholder() {
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = run_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_run(ctx).await.expect("register run");

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
    // The per-run token was stripped before forwarding.
    assert!(seen.headers.get(PROXY_TOKEN_HEADER).is_none());

    proxy.unregister_run(&token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_request_body_to_upstream() {
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = run_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_run(ctx).await.expect("register run");

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

    proxy.unregister_run(&token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_missing_and_wrong_token_without_calling_upstream() {
    let (mock_addr, captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = run_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_run(ctx).await.expect("register run");
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

    proxy.unregister_run(&token_hex).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_handshake_for_out_of_constraint_sni() {
    let (mock_addr, _captured) = start_mock_upstream().await;
    let proxy = proxy_for(mock_addr);
    let (ctx, _token, token_hex) = run_context(mock_addr.port());
    let ca_pem = ctx.ca.ca_cert_pem().to_string();
    let binding = proxy.register_run(ctx).await.expect("register run");

    // The guest trusts the per-run CA, but the CA is name-constrained to
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

    proxy.unregister_run(&token_hex).await;
}
