//! End-to-end credential-proxy provisioning test.
//!
//! Boots a real VM, stands up the host-side injection proxy plus a mock TLS
//! upstream, and provisions the guest the way a real run does: writes the
//! per-sandbox CA into the guest and stages the `/etc/hosts` redirect of the
//! upstream name to the gateway, which the real guest-agent mirrors into
//! `/etc/hosts` with its own privileged write. It asserts:
//! - the guest-agent mirrors the proxy's host aliases into the guest's
//!   `/etc/hosts` (observed guest state — the part that needs a real VM);
//! - the host-computed provisioning artifacts (env + staged CA file) carry no
//!   real key (a check on `build_guest_provisioning`'s output);
//! - a client presenting those same artifacts — per-sandbox CA, proxy token,
//!   base URL, placeholder key — authenticates through the proxy binding and has
//!   the real key injected upstream.
//!
//! Scope and limits: the injecting request is a host-side `reqwest` client, not
//! the guest's own HTTP client. The deterministic image ships busybox, whose
//! built-in TLS cannot complete a handshake with the rustls proxy, so the guest
//! cannot drive the call. This test therefore does not verify the guest client's
//! own env-to-header transform — that `ANTHROPIC_API_KEY` /
//! `ANTHROPIC_CUSTOM_HEADERS` / `ANTHROPIC_BASE_URL` become the right wire
//! headers, or that it loads the staged CA. Only a manual run with a real client
//! covers that. The proxy's injection mechanism (token check, key replacement,
//! token strip) is covered without a VM in `tests/proxy.rs`.
//!
//! ## Prerequisites
//!
//! ```bash
//! scripts/build_test_image.sh
//! VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//! VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//! cargo test --test e2e_credential_proxy -- --ignored --test-threads=1
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::header::HeaderMap;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use secrecy::SecretString;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use void_box::backend::{
    guest_host_gateway, BackendConfig, BackendSecurityConfig, GuestConsoleSink, VmmBackend,
};
use void_box::proxy::injector::{ApiKeyScheme, StaticApiKeyInjector};
use void_box::proxy::{
    assert_no_real_credential, build_guest_provisioning, render_guest_hosts, ProxiedUpstream,
    ProxyCa, ProxyHandle, ProxyToken, SandboxContext, GUEST_HOSTS_PATH, PROXY_TOKEN_HEADER,
};
use void_box_protocol::SessionSecret;

#[path = "../common/test_artifacts.rs"]
mod test_artifacts;

const UPSTREAM_HOST: &str = "api.anthropic.com";
const REAL_KEY: &str = "sk-ant-e2e-real-host-held-secret";

type CapturedHeaders = Arc<Mutex<Option<HeaderMap>>>;

/// Start the backend, or `None` when the host genuinely cannot virtualize (the
/// caller skips) — the skip-or-fail contract lives on [`test_artifacts::start_backend`].
async fn start_backend() -> Option<Box<dyn VmmBackend>> {
    test_artifacts::start_backend(backend_config(), "backend start (credential_proxy)").await
}

fn backend_config() -> BackendConfig {
    let (kernel, initramfs) = test_artifacts::artifacts();

    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("getrandom");

    BackendConfig {
        memory_mb: 256,
        vcpus: 1,
        kernel,
        initramfs: Some(initramfs),
        rootfs: None,
        network: true,
        enable_vsock: true,
        guest_console: GuestConsoleSink::Stderr,
        shared_dir: None,
        mounts: vec![],
        oci_rootfs: None,
        oci_rootfs_dev: None,
        oci_rootfs_disk: None,
        env: vec![],
        security: BackendSecurityConfig {
            session_secret: SessionSecret::new(secret),
            command_allowlist: vec!["sh".into(), "cat".into()],
            network_deny_list: vec!["169.254.0.0/16".into()],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
        snapshot: None,
        enable_snapshots: false,
    }
}

/// Mock TLS upstream recording request headers.
async fn start_mock_upstream() -> (SocketAddr, CapturedHeaders) {
    let cert =
        rcgen::generate_simple_self_signed(vec![UPSTREAM_HOST.to_string()]).expect("upstream cert");
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("upstream config");
    let acceptor = TlsAcceptor::from(Arc::new(config));

    // Bind loopback, not the 0.0.0.0 wildcard: the address is fed to reqwest's
    // `.resolve`, and a wildcard is a listen address, not a stable dial target
    // (connect(0.0.0.0) is not loopback on every platform).
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    let captured: CapturedHeaders = Arc::new(Mutex::new(None));

    let captured_task = captured.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let captured = captured_task.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let captured = captured.clone();
                    async move {
                        *captured.lock().unwrap() = Some(req.headers().clone());
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

async fn guest_sh(backend: &dyn VmmBackend, script: &str) -> void_box::ExecOutput {
    test_artifacts::expect_vm(
        backend
            .exec("sh", &["-c", script], &[], &[], None, Some(30))
            .await,
        "guest exec (credential_proxy)",
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
async fn provisioning_mirrors_hosts_and_injects_without_leaking_real_key() {
    let Some(backend) = start_backend().await else {
        return;
    };
    let (mock_addr, captured) = start_mock_upstream().await;

    // Proxy upstream client trusts the self-signed mock and routes the upstream
    // host to the mock address.
    let upstream_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve(UPSTREAM_HOST, mock_addr)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("upstream client");
    let proxy = ProxyHandle::new(upstream_client);

    let token = ProxyToken::generate();
    let ca = Arc::new(ProxyCa::generate(vec![UPSTREAM_HOST.to_string()]).expect("CA"));
    let ca_pem = ca.ca_cert_pem().to_string();
    let injector = Arc::new(StaticApiKeyInjector::new(
        UPSTREAM_HOST,
        ApiKeyScheme::AnthropicXApiKey,
        SecretString::from(REAL_KEY),
    ));
    let ctx = SandboxContext::new(token, ca, injector, vec![UPSTREAM_HOST.to_string()])
        .with_upstream_port(mock_addr.port());
    let binding = proxy.register_sandbox(ctx).await.expect("register sandbox");

    // Provision the guest: write the CA, redirect the upstream name to the
    // gateway, and assert no real key reaches the staged env/files.
    let upstream = ProxiedUpstream::for_provider(&void_box::llm::LlmProvider::Claude).unwrap();
    let provisioning = build_guest_provisioning(&upstream, &binding, &ca_pem, guest_host_gateway());
    assert_no_real_credential(
        &provisioning.env,
        std::slice::from_ref(&provisioning.ca_file),
        REAL_KEY,
    )
    .expect("no real credential in guest provisioning");

    backend
        .write_file(&provisioning.ca_file.0, provisioning.ca_file.1.as_bytes())
        .await
        .expect("write CA into guest");

    // Exercise the real host-side hosts provisioning (not a shell `echo`): stage
    // the rendered hosts file under /etc/voidbox, which the guest-agent mirrors
    // into /etc/hosts with its own privileged write.
    backend
        .mkdir_p("/etc/voidbox")
        .await
        .expect("mkdir /etc/voidbox");
    backend
        .write_file(
            GUEST_HOSTS_PATH,
            render_guest_hosts(&provisioning.host_aliases).as_bytes(),
        )
        .await
        .expect("stage proxy hosts");
    let out = guest_sh(&*backend, "cat /etc/hosts").await;
    for (ip, host) in &provisioning.host_aliases {
        assert!(
            out.stdout_str().contains(&format!("{ip} {host}")),
            "guest-agent did not mirror proxy alias '{ip} {host}' into /etc/hosts; \
             got: {}",
            out.stdout_str()
        );
    }

    // Present the guest's own provisioning artifacts — per-sandbox CA, proxy
    // token, base URL, placeholder key — from a host-side client with a working
    // TLS stack, and prove the proxy injects the real key. The request carries
    // the same SNI, proxy token, CA trust, and placeholder the guest would
    // present (the inputs the proxy keys on). It is not the guest's own client,
    // so its TLS stack and env-to-header behavior are out of scope here.
    let provisioned_token_header = provisioning
        .env
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_CUSTOM_HEADERS")
        .map(|(_, v)| v.clone())
        .expect("provisioning sets ANTHROPIC_CUSTOM_HEADERS");
    let (token_header_name, token_header_value) = provisioned_token_header
        .split_once(':')
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .expect("token header is 'name: value'");
    assert_eq!(token_header_name, PROXY_TOKEN_HEADER);
    let base_url = provisioning
        .env
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
        .map(|(_, v)| v.clone())
        .expect("provisioning sets ANTHROPIC_BASE_URL");
    // A regressed base URL (wrong host or port) must fail here, not be masked by
    // the explicit `.resolve` below that re-derives the port from `binding`.
    let parsed_base = reqwest::Url::parse(&base_url).expect("base URL parses");
    assert_eq!(
        parsed_base.host_str(),
        Some(UPSTREAM_HOST),
        "provisioned base URL host"
    );
    assert_eq!(
        parsed_base.port(),
        Some(binding.port),
        "provisioned base URL port"
    );

    // Trust the CA the guest was given (not `danger_accept_invalid_certs`), so a
    // broken CA chain fails the test, and route the upstream name to the binding
    // — the redirect the guest resolves via /etc/hosts.
    let ca_root = reqwest::Certificate::from_pem(ca_pem.as_bytes()).expect("parse provisioned CA");
    let guest_view = reqwest::Client::builder()
        .add_root_certificate(ca_root)
        .resolve(
            UPSTREAM_HOST,
            SocketAddr::from((Ipv4Addr::LOCALHOST, binding.port)),
        )
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("guest-view client");
    // The guest carries a non-secret placeholder key (provisioning sets it in the
    // guest env); the proxy replaces it with the host-held real key. Injection is
    // gated on the credential header already being present, so send the same
    // placeholder the guest was given, then assert the upstream saw the real key
    // (never the placeholder) and that the proxy token never leaked upstream.
    let placeholder_key = provisioning
        .env
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_API_KEY")
        .map(|(_, v)| v.clone())
        .expect("provisioning sets a placeholder ANTHROPIC_API_KEY");
    let resp = guest_view
        .get(format!("{base_url}/v1/messages"))
        .header(token_header_name.as_str(), token_header_value.as_str())
        .header("x-api-key", placeholder_key.as_str())
        .send()
        .await
        .expect("call through proxy binding");
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "proxy call status");
    assert_eq!(
        resp.text().await.expect("proxy response body").trim(),
        "upstream-ok"
    );

    let seen = captured.lock().unwrap().clone().expect("upstream called");
    let upstream_key = seen.get("x-api-key").expect("upstream saw x-api-key");
    assert_eq!(
        upstream_key, REAL_KEY,
        "proxy did not inject the host-held key"
    );
    assert_ne!(
        upstream_key,
        placeholder_key.as_str(),
        "guest placeholder key leaked upstream"
    );
    assert!(
        seen.get(PROXY_TOKEN_HEADER).is_none(),
        "proxy leaked its token upstream"
    );

    proxy.unregister_sandbox(&binding.token_hex).await;
}
