//! Real-upstream smoke test for the credential-injection proxy.
//!
//! `proxy.rs` drives the proxy against a mock upstream that accepts anything, so it
//! proves injection/auth/routing but not the last mile. This test re-originates
//! through the *production* upstream client (`start_proxy`) to the real
//! `api.anthropic.com` and asserts Anthropic accepts the injected request. It proves
//! what the mock cannot: the production client can complete real TLS to Anthropic,
//! Anthropic accepts the HTTP/1.1 request the proxy re-originates, and the host-held
//! `x-api-key` — never the guest's placeholder — authenticates.
//!
//! Ignored by default; requires a funded `ANTHROPIC_API_KEY` and outbound network.
//! Runs anywhere (no VM/KVM), including macOS. Run with:
//!
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... \
//!   cargo test --test proxy_real_upstream -- --ignored --nocapture
//! ```

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use secrecy::SecretString;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use void_box::proxy::injector::{ApiKeyScheme, StaticApiKeyInjector};
use void_box::proxy::{start_proxy, ProxyCa, ProxyToken, SandboxContext, PROXY_TOKEN_HEADER};

const UPSTREAM_HOST: &str = "api.anthropic.com";
/// The non-secret placeholder the guest carries, matching `ANTHROPIC_KEY_PLACEHOLDER`.
const PLACEHOLDER_KEY: &str = "voidbox-proxy-placeholder";
/// Minimal cheapest-model request; `max_tokens` kept tiny to keep the cost ~nil.
const BODY: &str = r#"{"model":"claude-haiku-4-5","max_tokens":16,"messages":[{"role":"user","content":"Reply with exactly: proxy check ok"}]}"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a funded ANTHROPIC_API_KEY; hits real api.anthropic.com"]
async fn injects_real_key_and_real_anthropic_accepts_it() {
    let Ok(real_key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!("SKIP: ANTHROPIC_API_KEY not set");
        return;
    };
    if real_key.trim().is_empty() {
        eprintln!("SKIP: ANTHROPIC_API_KEY is empty");
        return;
    }

    // Production upstream client: real TLS validation to Anthropic, SSRF guard on
    // resolution, no env proxy, no redirect-following. This is the exact wiring the
    // running system uses — no mock, no `danger_accept_invalid_certs`.
    let proxy = start_proxy().await.expect("start proxy");

    // Per-sandbox mechanisms: token (authentication), name-constrained CA (trust
    // scope), injector (the host-held real key for exactly this upstream).
    let token = ProxyToken::generate();
    let token_hex = token.to_hex();
    let ca = Arc::new(ProxyCa::generate(vec![UPSTREAM_HOST.to_string()]).expect("per-sandbox CA"));
    let ca_pem = ca.ca_cert_pem().to_string();
    let injector = Arc::new(StaticApiKeyInjector::new(
        UPSTREAM_HOST,
        ApiKeyScheme::AnthropicXApiKey,
        SecretString::from(real_key),
    ));
    // `upstream_port` defaults to 443 (real HTTPS) via `DEFAULT_UPSTREAM_PORT`.
    let ctx = SandboxContext::new(token.clone(), ca, injector, vec![UPSTREAM_HOST.to_string()]);
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    // Guest side: TLS to the loopback listener trusting *only* the per-sandbox CA,
    // carrying the placeholder key and the per-sandbox token — exactly what a
    // provisioned guest client sends.
    let connector = guest_connector(&ca_pem);
    let (status, body) = guest_request(&connector, binding.port, &token_hex, BODY.as_bytes())
        .await
        .expect("guest request through proxy");

    let body_text = String::from_utf8_lossy(&body);
    eprintln!("--- real upstream response ---");
    eprintln!("status: {status}");
    eprintln!("body:   {body_text}");
    eprintln!("------------------------------");

    assert_eq!(
        status,
        StatusCode::OK,
        "real Anthropic rejected the injected request: {body_text}"
    );
    // A successful Messages API call returns a `message` object.
    assert!(
        body_text.contains("\"type\":\"message\""),
        "expected a Messages API completion, got: {body_text}"
    );

    proxy.unregister_sandbox(&token_hex).await;
}

/// Build a guest-side TLS connector that trusts only `ca_pem` (the per-sandbox CA).
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

/// Send one Messages API request through the proxy as a provisioned guest would.
async fn guest_request(
    connector: &TlsConnector,
    proxy_port: u16,
    token_hex: &str,
    body: &[u8],
) -> std::io::Result<(StatusCode, Bytes)> {
    let tcp = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    let server_name = ServerName::try_from(UPSTREAM_HOST.to_string()).expect("server name");
    let tls = connector.connect(server_name, tcp).await?;

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(std::io::Error::other)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", UPSTREAM_HOST)
        .header("x-api-key", PLACEHOLDER_KEY)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .header(PROXY_TOKEN_HEADER, token_hex)
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
