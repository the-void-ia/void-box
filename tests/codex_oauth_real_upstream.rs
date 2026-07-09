//! V2 OAuth-acceptance harness (RFC-0002 rollout) for the codex ChatGPT path.
//!
//! `proxy.rs::codex_oauth_injects_minted_bearer_and_account_id` proves the
//! injection/refresh/write-back mechanics against mocks. This test proves what a
//! mock cannot — the feasibility unknowns the RFC gates codex-OAuth's real use
//! on (the codex leg of R4/R5/R13):
//!
//! - **R4**: OpenAI's token endpoint accepts a refresh grant *replayed by the
//!   host store* (not the genuine codex CLI), returns a usable access token, and
//!   rotates the refresh token — which the store writes back atomically.
//! - **R5**: the real ChatGPT codex backend accepts a *host-minted* Bearer plus
//!   the host-held `chatgpt-account-id` — the pass gate is that authentication
//!   succeeds (no 401/403); the request body is a minimal Responses call whose
//!   shape the backend may still reject with a 4xx, which does not indicate an
//!   auth failure.
//!
//! # This consumes a single-use refresh token
//!
//! Running it spends the refresh token in the supplied `auth.json` and rotates
//! it — so the file is mutated and any *other* client using it (e.g. a real
//! `codex login`) is invalidated. Use a **throwaway** ChatGPT account, never
//! your primary login. For that reason it is gated behind both `#[ignore]` and
//! an explicit `VOIDBOX_V2_CODEX_OAUTH=1` opt-in.
//!
//! Runs anywhere (no VM/KVM), including macOS. Run with:
//!
//! ```bash
//! VOIDBOX_V2_CODEX_OAUTH=1 \
//! VOIDBOX_TEST_CODEX_AUTH=/path/to/throwaway/auth.json \
//!   cargo test --test codex_oauth_real_upstream -- --ignored --nocapture
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
use void_box::credentials::{OAuthProviderKind, OAuthTokenStore};
use void_box::proxy::{
    start_proxy, OAuthBearerInjector, ProxyCa, ProxyToken, SandboxContext,
    CHATGPT_ACCOUNT_ID_HEADER, PROXY_TOKEN_HEADER,
};

const UPSTREAM_HOST: &str = "chatgpt.com";
/// The non-secret placeholder the guest carries, matching `ANTHROPIC_KEY_PLACEHOLDER`.
const PLACEHOLDER_TOKEN: &str = "voidbox-proxy-placeholder";
/// Minimal Responses API request. The auth gate (R5) is status ≠ 401/403; the
/// backend may still 4xx the body shape without that meaning an auth failure.
const BODY: &str = r#"{"model":"gpt-5-codex","instructions":"reply ok","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with exactly: proxy check ok"}]}],"stream":true,"store":false}"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "V2: consumes a single-use refresh token; requires VOIDBOX_V2_CODEX_OAUTH=1 + a throwaway account's auth.json"]
async fn host_refreshed_bearer_is_accepted_by_real_chatgpt_backend() {
    if std::env::var("VOIDBOX_V2_CODEX_OAUTH").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP: set VOIDBOX_V2_CODEX_OAUTH=1 to opt in (this rotates a single-use refresh token)"
        );
        return;
    }
    let Ok(auth_path) = std::env::var("VOIDBOX_TEST_CODEX_AUTH") else {
        eprintln!("SKIP: VOIDBOX_TEST_CODEX_AUTH not set (path to a throwaway auth.json)");
        return;
    };
    let auth_path = PathBuf::from(auth_path);
    let Ok(auth_json) = std::fs::read_to_string(&auth_path) else {
        eprintln!("SKIP: cannot read {}", auth_path.display());
        return;
    };

    // The store uses the production token endpoint + SSRF-guarded client — the
    // exact wiring the running system uses. Write-back targets the supplied file.
    let store = Arc::new(
        OAuthTokenStore::from_json(
            OAuthProviderKind::CodexChatGpt,
            &SecretString::from(auth_json),
            auth_path.clone(),
        )
        .expect("build store from throwaway auth.json"),
    );
    let account_id = store
        .codex_account_id()
        .await
        .expect("auth.json must carry tokens.account_id (re-run 'codex login')");

    // --- R4: a host-replayed refresh is accepted and the rotated token persisted.
    let refresh_before = read_refresh_token(&auth_path);
    let minted = store
        .access_token()
        .await
        .expect("host-side OAuth refresh should mint an access token (R4)");
    assert!(
        !minted.expose_secret().is_empty(),
        "minted access token must be non-empty"
    );
    let refresh_after = read_refresh_token(&auth_path);
    eprintln!("--- R4: refresh ---");
    eprintln!("refresh token rotated: {}", refresh_before != refresh_after);
    eprintln!("-------------------");

    // --- R5: the minted Bearer + account id authenticate against the real
    // backend through the production proxy.
    let proxy = start_proxy()
        .await
        .expect("start proxy")
        .with_loopback_bind();
    let token = ProxyToken::generate();
    let token_hex = token.to_hex();
    let ca = Arc::new(ProxyCa::generate(vec![UPSTREAM_HOST.to_string()]).expect("per-sandbox CA"));
    let ca_pem = ca.ca_cert_pem().to_string();
    let injector = Arc::new(
        OAuthBearerInjector::new(UPSTREAM_HOST, store)
            .with_extra_header(CHATGPT_ACCOUNT_ID_HEADER, account_id),
    );
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

    assert!(
        status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
        "real ChatGPT backend rejected the host-minted Bearer + account id \
         (401/403 = token binding/attestation, R13): {body_text}"
    );

    proxy.unregister_sandbox(&token_hex).await;
}

/// Read the current `tokens.refresh_token` from an auth.json, for the rotation
/// check. Returns an empty string if it cannot be read.
fn read_refresh_token(path: &PathBuf) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    doc["tokens"]["refresh_token"]
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

/// Send one Responses API request through the proxy as a provisioned codex
/// guest would: a placeholder Bearer and placeholder account id (the proxy
/// replaces both), codex's `originator`, and the per-sandbox token.
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
        .uri("/backend-api/codex/responses")
        .header("host", UPSTREAM_HOST)
        .header("authorization", format!("Bearer {PLACEHOLDER_TOKEN}"))
        .header(CHATGPT_ACCOUNT_ID_HEADER, PLACEHOLDER_TOKEN)
        .header("originator", "codex_cli_rs")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
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
