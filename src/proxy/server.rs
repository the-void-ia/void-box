//! Proxy server: one shared process, one TLS-terminating listener per sandbox.
//!
//! # Why a listener per sandbox, not one shared listener
//!
//! The proxy must pick the *per-sandbox* CA to terminate a guest's TLS before any
//! HTTP byte (and therefore the per-sandbox token) is readable — the CA choice
//! happens at the TLS ClientHello. Guests all appear to arrive from the same
//! SLIRP gateway address, so the only pre-TLS discriminator is the destination
//! port. Each sandbox therefore gets its own ephemeral port (and its own CA); the
//! token is still checked on the HTTP layer as a neighbour guard and stripped
//! before forwarding. This stays "one shared process" — the listeners are tasks
//! inside it, so the memory cost is per-sandbox state, not a per-sandbox OS process.
//!
//! # Request flow (the frozen pipeline, see [`crate::proxy`])
//!
//! TLS-terminate (per-sandbox CA, SNI → upstream host) → auth (per-sandbox token) →
//! policy (allow/deny) → inject credential header → re-originate to the real
//! upstream over fresh TLS, streaming the body through without inspection.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use http::header::{
    HeaderMap, HeaderName, AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http::{Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::backend::credential_proxy_bind_addr;
use crate::error::{Error, Result};
use crate::proxy::ssrf::SsrfGuardResolver;
use crate::proxy::{
    EgressEvent, InjectOutcome, ProxyToken, SandboxContext, PROXY_TOKEN_BEARER_PREFIX,
    PROXY_TOKEN_HEADER,
};

/// Cap on hyper's per-connection read buffer (headers + request-line). Bounds
/// the host memory a single guest connection can pin in the parser before any
/// resource accounting (R10: strict size limits on the guest-controlled parser
/// surface). Bodies stream and are not held in this buffer.
const MAX_HEADER_BUF_BYTES: usize = 64 * 1024;

/// Audit-event reason recorded when a protocol-upgrade request is refused (R8).
const REASON_UPGRADE_REFUSED: &str = "websocket-upgrade-refused";

/// Response body for a refused protocol upgrade. Names the downstream symptom
/// and the remediation so an operator who sees it in a client log can connect it
/// to the provisioning knob without a debugging session.
const UPGRADE_REFUSED_MESSAGE: &str = "protocol upgrade (WebSocket) is not supported by the \
     credential proxy; the provisioned client config forces plain HTTPS \
     (supports_websockets = false)";

/// Body type the proxy hands back to hyper: a boxed stream of bytes whose error
/// is normalised to `std::io::Error`.
type ProxyBody = BoxBody<Bytes, std::io::Error>;

/// What [`ProxyHandle::register_sandbox`] returns: how the guest reaches this sandbox's
/// proxy listener and the token it must present.
#[derive(Clone)]
pub struct SandboxBinding {
    /// Host-side port the sandbox's listener bound to (reachable from the guest via
    /// the SLIRP/NAT gateway).
    pub port: u16,
    /// Per-sandbox token, hex-encoded, for the guest to present on each connection.
    pub token_hex: String,
}

impl std::fmt::Debug for SandboxBinding {
    /// Redacts `token_hex` so a `{:?}` of a binding never lands the per-sandbox
    /// token in a log, mirroring [`ProxyToken`]'s redacting `Debug` (R15).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxBinding")
            .field("port", &self.port)
            .field("token_hex", &"<redacted>")
            .finish()
    }
}

/// One registered run: its listener task and a shutdown signal.
struct SandboxSlot {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    port: u16,
}

/// Handle to the proxy: it owns the per-sandbox listener tasks and the shared
/// upstream client. The interface supports registering and unregistering several
/// sandboxes on one handle, but M0 stands up a handle per agent run in
/// [`crate::agent_box`] and registers that run's single sandbox — the
/// "one shared proxy process" of ADR-0003 is a later milestone.
pub struct ProxyHandle {
    sandboxes: Arc<Mutex<HashMap<String, SandboxSlot>>>,
    upstream: reqwest::Client,
    /// IP each per-sandbox listener binds. Production ([`start_proxy`]) uses the
    /// guest-reachable address ([`credential_proxy_bind_addr`]); an in-process
    /// test with no VM overrides it to loopback via [`Self::with_loopback_bind`]
    /// (on macOS the guest-reachable address is the VZ NAT gateway, which does
    /// not exist without a running VM).
    bind_ip: IpAddr,
}

impl ProxyHandle {
    /// Build a handle that re-originates upstream requests with `upstream`,
    /// binding listeners on the guest-reachable address. Exposed so tests can
    /// supply a client whose DNS/trust is pointed at a mock upstream; production
    /// uses [`start_proxy`].
    pub fn new(upstream: reqwest::Client) -> Self {
        Self {
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
            upstream,
            bind_ip: credential_proxy_bind_addr(0).ip(),
        }
    }

    /// Bind listeners on loopback instead of the guest-reachable address. For
    /// in-process tests with no VM: their clients connect over `127.0.0.1`, and
    /// on macOS the guest-reachable address (the VZ NAT gateway) is unbindable
    /// without a running VM. Never use in production — the guest cannot reach a
    /// loopback listener on macOS/VZ.
    pub fn with_loopback_bind(mut self) -> Self {
        self.bind_ip = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        self
    }

    /// Register a sandbox: bind a fresh per-sandbox listener, spawn its accept loop, and
    /// return the guest-reachable port + the token to present.
    pub async fn register_sandbox(&self, ctx: SandboxContext) -> Result<SandboxBinding> {
        let token_hex = ctx.token.to_hex();
        let server_config = ctx.ca.server_config();
        let acceptor = TlsAcceptor::from(server_config);

        let bind_addr = SocketAddr::new(self.bind_ip, 0);
        let listener = TcpListener::bind(bind_addr).await.map_err(|e| {
            let macos_hint = if cfg!(target_os = "macos") && !self.bind_ip.is_loopback() {
                " (the VZ NAT gateway address exists only while a VZ NAT VM is \
                 running — the proxy must be registered after guest boot)"
            } else {
                ""
            };
            Error::Network(format!(
                "credential proxy listener bind failed on {bind_addr}: {e}{macos_hint}"
            ))
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Network(format!("proxy listener addr failed: {e}")))?
            .port();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let ctx = Arc::new(ctx);
        let upstream = self.upstream.clone();
        let task = tokio::spawn(sandbox_listener(
            listener,
            acceptor,
            ctx,
            upstream,
            shutdown_rx,
        ));

        self.sandboxes.lock().await.insert(
            token_hex.clone(),
            SandboxSlot {
                shutdown: shutdown_tx,
                task,
                port,
            },
        );
        info!(port, "proxy sandbox registered");
        Ok(SandboxBinding { port, token_hex })
    }

    /// Stop and drop a registered sandbox's listener.
    pub async fn unregister_sandbox(&self, token_hex: &str) {
        if let Some(slot) = self.sandboxes.lock().await.remove(token_hex) {
            let _ = slot.shutdown.send(true);
            slot.task.abort();
            debug!(port = slot.port, "proxy sandbox unregistered");
        }
    }
}

/// Start the shared proxy with a production upstream client. The client never
/// follows redirects (credentials must not chase an agent-controlled redirect,
/// R3) and resolves upstream names through the [`SsrfGuardResolver`], which
/// rejects any name resolving to an internal address (R3).
///
/// `no_proxy()` is deliberate: with a host `HTTPS_PROXY`/`ALL_PROXY` set, reqwest
/// would `CONNECT` through it and skip its own resolver, silently bypassing the
/// SSRF guard and routing the injected key through an unexpected proxy. Chaining
/// to a corporate egress proxy is a deliberate future feature, not an env-var
/// side effect.
pub async fn start_proxy() -> Result<ProxyHandle> {
    let upstream = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(SsrfGuardResolver))
        .build()
        .map_err(|e| Error::Network(format!("proxy upstream client build failed: {e}")))?;
    Ok(ProxyHandle::new(upstream))
}

/// Per-sandbox accept loop: terminate TLS, then serve HTTP/1 over each connection.
async fn sandbox_listener(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    ctx: Arc<SandboxContext>,
    upstream: reqwest::Client,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!("proxy accept error: {e}");
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let ctx = ctx.clone();
                let upstream = upstream.clone();
                // Each connection races serving against the sandbox's shutdown so an
                // in-flight (possibly long-lived SSE) connection — and the
                // credential-holding SandboxContext it pins — is dropped when the sandbox
                // is unregistered, not left running until the guest hangs up.
                let mut conn_shutdown = shutdown_rx.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        result = serve_connection(stream, acceptor, ctx, upstream) => {
                            if let Err(e) = result {
                                debug!("proxy connection ended: {e}");
                            }
                        }
                        _ = conn_shutdown.changed() => {
                            debug!("proxy connection cancelled by run shutdown");
                        }
                    }
                });
            }
        }
    }
}

/// Terminate one guest TLS connection and serve its HTTP requests.
async fn serve_connection(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    ctx: Arc<SandboxContext>,
    upstream: reqwest::Client,
) -> Result<()> {
    let tls = acceptor
        .accept(stream)
        .await
        .map_err(|e| Error::Network(format!("proxy TLS handshake failed: {e}")))?;

    // The SNI names the upstream. Refuse anything outside the sandbox's allow-set;
    // the per-sandbox CA would refuse to mint a leaf for it anyway, but checking
    // here keeps the refusal explicit and audited.
    let host = match tls.get_ref().1.server_name() {
        Some(name) if ctx.permits_upstream(name) => Arc::<str>::from(name),
        other => {
            warn!(sni = ?other, "proxy refusing connection: SNI not in run allow-set");
            return Ok(());
        }
    };

    let io = TokioIo::new(tls);
    let service = service_fn(move |req: Request<Incoming>| {
        let ctx = ctx.clone();
        let upstream = upstream.clone();
        let host = host.clone();
        async move { Ok::<_, Infallible>(proxy_request(req, ctx, host, upstream).await) }
    });

    http1::Builder::new()
        .max_buf_size(MAX_HEADER_BUF_BYTES)
        .serve_connection(io, service)
        .await
        .map_err(|e| Error::Network(format!("proxy HTTP serve failed: {e}")))
}

/// Apply the pipeline to one request and re-originate it upstream. Always
/// resolves to a `Response` — failures become HTTP error responses, never a
/// dropped connection, so the client sees a clean status.
async fn proxy_request(
    req: Request<Incoming>,
    ctx: Arc<SandboxContext>,
    host: Arc<str>,
    upstream: reqwest::Client,
) -> Response<ProxyBody> {
    // --- auth: per-sandbox token, checked then stripped ---
    let presented = presented_proxy_token(req.headers());
    let authed = matches!(&presented, Some(token) if ctx.token.matches(token));
    if !authed {
        return text_response(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            "missing or invalid proxy token",
        );
    }

    // --- protocol: refuse upgrades explicitly (R8) ---
    // The proxy speaks plain request/response HTTP; it cannot inject into a
    // WebSocket upgrade and must not answer one by silently stripping the
    // Upgrade header — the client would treat the plain response as a failed
    // handshake with no hint why. Refuse with a distinguishable status before
    // any credential is touched or upstream connection made. Placed after auth
    // so only the legitimate guest learns proxy behavior; strangers still get 407.
    if is_upgrade_request(req.headers()) {
        ctx.audit.record(EgressEvent {
            host: host.to_string(),
            port: ctx.upstream_port,
            allowed: false,
            injected: false,
            reason: Some(REASON_UPGRADE_REFUSED),
        });
        warn!(host = %host, "proxy: refusing protocol-upgrade request (WebSocket unsupported, R8)");
        return text_response(StatusCode::BAD_GATEWAY, UPGRADE_REFUSED_MESSAGE);
    }

    // --- policy: reachability ---
    if let crate::proxy::Decision::Deny { reason } = ctx.policy.decision(&host) {
        ctx.audit.record(EgressEvent {
            host: host.to_string(),
            port: ctx.upstream_port,
            allowed: false,
            injected: false,
            reason: Some("policy-denied"),
        });
        return text_response(StatusCode::FORBIDDEN, &format!("egress denied: {reason}"));
    }

    let (parts, body) = req.into_parts();
    let mut headers = parts.headers;
    strip_hop_by_hop(&mut headers);
    headers.remove(HeaderName::from_static(PROXY_TOKEN_HEADER));
    // A token-bearing `Authorization` (`Bearer voidbox-proxy-…`) is a carrier,
    // never a credential, so it is dropped here unconditionally: injection
    // replaces `Authorization` for owned hosts anyway, and this keeps the token
    // off the wire even for a host no injector owns.
    let authorization_carries_token = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|bearer| bearer.starts_with(PROXY_TOKEN_BEARER_PREFIX));
    if authorization_carries_token {
        headers.remove(AUTHORIZATION);
    }
    headers.remove(HOST);
    // The body is re-originated as an unknown-length stream (chunked), so drop
    // any inbound Content-Length: forwarding it alongside chunked framing is the
    // ambiguity that invites request-smuggling-class bugs.
    headers.remove(CONTENT_LENGTH);

    // --- inject: rewrite the credential header for this exact host ---
    // A failed injection for a host the injector owns (e.g. a malformed
    // host-held key) must fail closed: forwarding the request without the
    // credential would send an unauthenticated call upstream and mis-record it as
    // credentialed. Return 502 and audit `injected: false` instead.
    let injected = match ctx.injector.inject(&host, &mut headers).await {
        InjectOutcome::Injected => true,
        InjectOutcome::NotOwned => false,
        InjectOutcome::Failed => {
            ctx.audit.record(EgressEvent {
                host: host.to_string(),
                port: ctx.upstream_port,
                allowed: true,
                injected: false,
                reason: Some("credential-injection-failed"),
            });
            warn!(host = %host, "proxy: credential injection failed; refusing to forward uncredentialed");
            return text_response(StatusCode::BAD_GATEWAY, "credential injection failed");
        }
    };

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("https://{host}:{}{path_and_query}", ctx.upstream_port);

    let body_stream = body.into_data_stream();
    let upstream_body = reqwest::Body::wrap_stream(body_stream);

    let response = upstream
        .request(parts.method, &url)
        .headers(headers)
        .body(upstream_body)
        .send()
        .await;

    ctx.audit.record(EgressEvent {
        host: host.to_string(),
        port: ctx.upstream_port,
        allowed: true,
        injected,
        reason: None,
    });

    match response {
        Ok(upstream_resp) => relay_upstream_response(upstream_resp),
        Err(e) => {
            warn!(host = %host, "proxy upstream request failed: {e}");
            text_response(StatusCode::BAD_GATEWAY, "upstream request failed")
        }
    }
}

/// Convert a reqwest upstream response into a streaming hyper response,
/// dropping hop-by-hop headers.
fn relay_upstream_response(upstream_resp: reqwest::Response) -> Response<ProxyBody> {
    let status = upstream_resp.status();

    let mut builder = Response::builder().status(status);
    if let Some(out_headers) = builder.headers_mut() {
        for (name, value) in upstream_resp.headers() {
            if !is_hop_by_hop(name) {
                out_headers.append(name.clone(), value.clone());
            }
        }
    }

    let data_stream = upstream_resp
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    let body = StreamBody::new(data_stream).boxed();

    builder
        .body(body)
        .unwrap_or_else(|_| text_response(StatusCode::BAD_GATEWAY, "malformed upstream response"))
}

/// Build a plain-text response with a boxed body.
fn text_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(message.to_owned()))
        .map_err(|never| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body)
        .expect("static text response is always valid")
}

/// Extract the per-sandbox token a guest client presented, from either carrier:
/// the dedicated header (`x-voidbox-proxy-token`, the Claude path via
/// `ANTHROPIC_CUSTOM_HEADERS`), or embedded in the Bearer placeholder
/// (`Authorization: Bearer voidbox-proxy-<hex>`, the codex API-key path, whose
/// client has no custom-header env knob). The dedicated header wins when both
/// are present. Whatever carried the token never reaches the upstream: the
/// dedicated header is stripped, and `Authorization` is replaced or dropped by
/// the injection stage on every owned-host outcome.
fn presented_proxy_token(headers: &HeaderMap) -> Option<ProxyToken> {
    if let Some(token) = headers
        .get(PROXY_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(ProxyToken::from_hex)
    {
        return Some(token);
    }
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|bearer| bearer.strip_prefix(PROXY_TOKEN_BEARER_PREFIX))
        .and_then(ProxyToken::from_hex)
}

/// Whether the request asks for a protocol upgrade (RFC 9110 §7.8): an `Upgrade`
/// header, or a `Connection` header carrying the `upgrade` option. Checking both
/// matters — the `Connection: upgrade` option is what marks the `Upgrade` header
/// as hop-by-hop, and a client may send either in any casing or as one item of a
/// comma-separated list.
fn is_upgrade_request(headers: &HeaderMap) -> bool {
    if headers.contains_key(UPGRADE) {
        return true;
    }
    headers.get_all(CONNECTION).iter().any(|value| {
        value
            .to_str()
            .map(|options| {
                options
                    .split(',')
                    .any(|option| option.trim().eq_ignore_ascii_case("upgrade"))
            })
            .unwrap_or(false)
    })
}

/// Whether `name` is a hop-by-hop header that must not be forwarded across the
/// proxy boundary (RFC 7230 §6.1, plus the proxy token). `Upgrade` appears here
/// only as defensive stripping on the response path; a *request* that needs the
/// upgrade is refused explicitly by [`is_upgrade_request`] instead of being
/// silently downgraded (R8).
fn is_hop_by_hop(name: &HeaderName) -> bool {
    name == CONNECTION
        || name == TE
        || name == TRAILER
        || name == TRANSFER_ENCODING
        || name == UPGRADE
        || name.as_str().eq_ignore_ascii_case("keep-alive")
        || name.as_str().eq_ignore_ascii_case("proxy-authenticate")
        || name.as_str().eq_ignore_ascii_case("proxy-authorization")
        || name.as_str().eq_ignore_ascii_case(PROXY_TOKEN_HEADER)
}

/// Strip hop-by-hop headers from a request header map in place.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let to_remove: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_hop_by_hop(name))
        .cloned()
        .collect();
    for name in to_remove {
        headers.remove(name);
    }
}
