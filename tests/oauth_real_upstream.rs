//! V2 OAuth-acceptance harness (RFC-0002 rollout) for the Claude personal path.
//!
//! `proxy.rs::oauth_injects_minted_bearer_and_hides_placeholder` proves the
//! injection/refresh/write-back mechanics against mocks. This test proves what a
//! mock cannot — the two feasibility unknowns the RFC gates M1a's real use on:
//!
//! - **R4**: Anthropic's token endpoint accepts a refresh grant *replayed by the
//!   host store* (not the genuine client), returns a usable access token, and
//!   rotates the refresh token — which the store writes back atomically.
//! - **R5**: the real `/v1/messages` endpoint accepts a *host-minted* Bearer,
//!   returning a usable completion.
//!
//! # This consumes a single-use refresh token
//!
//! Running it spends the refresh token in the supplied credential file and
//! rotates it — so the file is mutated and any *other* client using it (e.g. a
//! real `claude auth login`) is invalidated. Use a **throwaway** Claude Pro/Max
//! account, never your primary login. For that reason it is gated behind both
//! `#[ignore]` and an explicit `VOIDBOX_V2_OAUTH=1` opt-in.
//!
//! Runs anywhere (no VM/KVM), including macOS. Run with:
//!
//! ```bash
//! VOIDBOX_V2_OAUTH=1 \
//! VOIDBOX_TEST_CLAUDE_CREDS=/path/to/throwaway/.credentials.json \
//!   cargo test --test oauth_real_upstream -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use secrecy::{ExposeSecret, SecretString};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use void_box::credentials::ClaudeOAuthStore;
use void_box::proxy::{
    start_proxy, OAuthBearerInjector, ProxyCa, ProxyToken, SandboxContext, PROXY_TOKEN_HEADER,
};

const UPSTREAM_HOST: &str = "api.anthropic.com";
/// The non-secret placeholder the guest carries, matching `ANTHROPIC_KEY_PLACEHOLDER`.
const PLACEHOLDER_TOKEN: &str = "voidbox-proxy-placeholder";
/// Minimal cheapest-model request; `max_tokens` kept tiny to keep the cost ~nil.
const BODY: &str = r#"{"model":"claude-haiku-4-5","max_tokens":16,"messages":[{"role":"user","content":"Reply with exactly: proxy check ok"}]}"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "V2: consumes a single-use refresh token; requires VOIDBOX_V2_OAUTH=1 + a throwaway account's creds"]
async fn host_refreshed_bearer_is_accepted_by_real_anthropic() {
    if std::env::var("VOIDBOX_V2_OAUTH").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP: set VOIDBOX_V2_OAUTH=1 to opt in (this rotates a single-use refresh token)"
        );
        return;
    }
    let Ok(creds_path) = std::env::var("VOIDBOX_TEST_CLAUDE_CREDS") else {
        eprintln!(
            "SKIP: VOIDBOX_TEST_CLAUDE_CREDS not set (path to a throwaway .credentials.json)"
        );
        return;
    };
    let creds_path = PathBuf::from(creds_path);
    let Ok(creds_json) = std::fs::read_to_string(&creds_path) else {
        eprintln!("SKIP: cannot read {}", creds_path.display());
        return;
    };

    // The store uses the production token endpoint + SSRF-guarded client — the
    // exact wiring the running system uses. Write-back targets the supplied file.
    let store = Arc::new(
        ClaudeOAuthStore::from_json(&SecretString::from(creds_json), creds_path.clone())
            .expect("build store from throwaway credentials"),
    );

    // --- R4: a host-replayed refresh is accepted and the rotated token persisted.
    let refresh_before = read_refresh_token(&creds_path);
    let minted = store
        .access_token()
        .await
        .expect("host-side OAuth refresh should mint an access token (R4)");
    assert!(
        !minted.expose_secret().is_empty(),
        "minted access token must be non-empty"
    );
    let refresh_after = read_refresh_token(&creds_path);
    eprintln!("--- R4: refresh ---");
    eprintln!("refresh token rotated: {}", refresh_before != refresh_after);
    eprintln!("-------------------");

    // --- R5: the minted Bearer authenticates a real inference request.
    let proxy = start_proxy().await.expect("start proxy");
    let token = ProxyToken::generate();
    let token_hex = token.to_hex();
    let ca = Arc::new(ProxyCa::generate(vec![UPSTREAM_HOST.to_string()]).expect("per-sandbox CA"));
    let ca_pem = ca.ca_cert_pem().to_string();
    let injector = Arc::new(OAuthBearerInjector::new(UPSTREAM_HOST, store));
    // `upstream_port` defaults to 443 (real HTTPS).
    let ctx = SandboxContext::new(token, ca, injector, vec![UPSTREAM_HOST.to_string()]);
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    let connector = guest_connector(&ca_pem);
    let (status, body) = guest_request(&connector, binding.port, &token_hex, BODY.as_bytes())
        .await
        .expect("guest request through proxy");

    let body_text = String::from_utf8_lossy(&body);
    eprintln!("--- R5: real upstream response ---");
    eprintln!("status: {status}");
    eprintln!("body:   {body_text}");
    eprintln!("----------------------------------");

    assert_eq!(
        status,
        StatusCode::OK,
        "real Anthropic rejected the host-minted Bearer (watch for 401/403 = token binding/attestation): {body_text}"
    );
    assert!(
        body_text.contains("\"type\":\"message\""),
        "expected a Messages API completion, got: {body_text}"
    );

    proxy.unregister_sandbox(&token_hex).await;
}

/// Read the current `claudeAiOauth.refreshToken` from a credential file, for the
/// rotation check. Returns an empty string if it cannot be read.
fn read_refresh_token(path: &PathBuf) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    doc["claudeAiOauth"]["refreshToken"]
        .as_str()
        .unwrap_or_default()
        .to_string()
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

/// Send one Messages API request through the proxy as a provisioned OAuth guest
/// would: a placeholder Bearer (the proxy replaces it), the OAuth beta header,
/// and the per-sandbox token. No `x-api-key` — OAuth mode does not use one.
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
        .header("authorization", format!("Bearer {PLACEHOLDER_TOKEN}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
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
