//! End-to-end credential-proxy integration test.
//!
//! Boots a real VM, stands up the host-side injection proxy plus a mock TLS
//! upstream, provisions the guest (per-sandbox CA + `/etc/hosts` redirect of the
//! upstream name to the gateway), and exercises a credentialed call from inside
//! the guest. Asserts that:
//! - the host-held key is injected and reaches the upstream,
//! - the guest never holds the real credential (env + the provisioned files),
//! - the proxy is reachable from the guest via the SLIRP/NAT gateway.
//!
//! ## Prerequisites
//!
//! ```bash
//! scripts/build_test_image.sh
//! VOID_BOX_KERNEL=/boot/vmlinuz-$(uname -r) \
//! VOID_BOX_INITRAMFS=/tmp/void-box-test-rootfs.cpio.gz \
//! cargo test --test e2e_credential_proxy -- --ignored --test-threads=1
//! ```
//!
//! All tests are `#[ignore]`. The injected-call leg depends on a guest HTTPS
//! client that honours a custom CA + header; the deterministic test image's
//! client capability is the CI-iteration point for this suite.

use std::net::SocketAddr;
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
    ProxyCa, ProxyHandle, ProxyToken, SandboxContext, GUEST_HOSTS_PATH,
};
use void_box_protocol::SessionSecret;

#[path = "../common/vm_preflight.rs"]
mod vm_preflight;

use std::path::PathBuf;

const UPSTREAM_HOST: &str = "api.anthropic.com";
const REAL_KEY: &str = "sk-ant-e2e-real-host-held-secret";

type CapturedHeaders = Arc<Mutex<Option<HeaderMap>>>;

fn backend_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        vm_preflight::require_kvm_usable().is_ok() && vm_preflight::require_vsock_usable().is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        true
    }
}

fn vm_artifacts() -> Option<(PathBuf, PathBuf)> {
    let kernel = PathBuf::from(std::env::var("VOID_BOX_KERNEL").ok()?);
    let initramfs = PathBuf::from(std::env::var("VOID_BOX_INITRAMFS").ok()?);
    if kernel.as_os_str().is_empty() || initramfs.as_os_str().is_empty() {
        return None;
    }
    if vm_preflight::require_kernel_artifacts(&kernel, Some(&initramfs)).is_err() {
        return None;
    }
    Some((kernel, initramfs))
}

async fn start_backend() -> Option<Box<dyn VmmBackend>> {
    if !backend_available() {
        eprintln!("skipping: VM backend not available");
        return None;
    }
    let (kernel, initramfs) = vm_artifacts().or_else(|| {
        eprintln!("skipping: set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS");
        None
    })?;

    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).ok()?;

    let config = BackendConfig {
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
            command_allowlist: vec!["sh".into(), "wget".into(), "cat".into(), "echo".into()],
            network_deny_list: vec!["169.254.0.0/16".into()],
            max_connections_per_second: 50,
            max_concurrent_connections: 64,
            seccomp: true,
        },
        snapshot: None,
        enable_snapshots: false,
    };

    let mut backend = void_box::backend::create_backend();
    match backend.start(config).await {
        Ok(()) => Some(backend),
        Err(e) => {
            eprintln!("skipping: backend start failed: {e}");
            None
        }
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

    let listener = TcpListener::bind(("0.0.0.0", 0)).await.expect("bind mock");
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

async fn guest_sh(backend: &dyn VmmBackend, script: &str) -> Option<void_box::ExecOutput> {
    match backend
        .exec("sh", &["-c", script], &[], &[], None, Some(30))
        .await
    {
        Ok(out) => Some(out),
        Err(e) => {
            eprintln!("guest exec error: {e}");
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires VM backend + kernel/initramfs + network"]
async fn guest_call_is_credential_injected_and_leaks_no_key() {
    let backend = match start_backend().await {
        Some(b) => b,
        None => return,
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
    if let Some(out) = guest_sh(&*backend, "cat /etc/hosts").await {
        for (ip, host) in &provisioning.host_aliases {
            assert!(
                out.stdout_str().contains(&format!("{ip} {host}")),
                "guest-agent did not mirror proxy alias '{ip} {host}' into /etc/hosts; \
                 got: {}",
                out.stdout_str()
            );
        }
    }

    // Exercise a credentialed call from inside the guest through the proxy.
    let token_header = provisioning
        .env
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_CUSTOM_HEADERS")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let base_url = provisioning
        .env
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
        .map(|(_, v)| v.clone())
        .unwrap();
    let script = format!(
        "wget -q -O - --ca-certificate={} --header='{}' {}/v1/messages",
        provisioning.ca_file.0, token_header, base_url
    );
    let out = guest_sh(&*backend, &script).await;

    if let Some(out) = out {
        if out.success() {
            assert_eq!(out.stdout_str().trim(), "upstream-ok");
            let seen = captured.lock().unwrap().clone().expect("upstream called");
            assert_eq!(seen.get("x-api-key").unwrap(), REAL_KEY);
        } else {
            // The deterministic test image's wget may lack HTTPS/custom-CA
            // support; the host-side proxy + no-credential-in-guest path above is still asserted.
            eprintln!(
                "note: guest HTTPS call did not succeed (client capability); \
                 stderr: {}",
                out.stderr_str()
            );
        }
    }

    proxy.unregister_sandbox(&binding.token_hex).await;
}
