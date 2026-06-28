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
use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use http::header::{
    HeaderMap, HeaderName, CONNECTION, CONTENT_LENGTH, HOST, TE, TRAILER, TRANSFER_ENCODING,
    UPGRADE,
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

use crate::backend::guest_accessible_bind_addr;
use crate::error::{Error, Result};
use crate::proxy::ssrf::SsrfGuardResolver;
use crate::proxy::{EgressEvent, ProxyToken, SandboxContext, PROXY_TOKEN_HEADER};

/// Cap on hyper's per-connection read buffer (headers + request-line). Bounds
/// the host memory a single guest connection can pin in the parser before any
/// resource accounting (R10: strict size limits on the guest-controlled parser
/// surface). Bodies stream and are not held in this buffer.
const MAX_HEADER_BUF_BYTES: usize = 64 * 1024;

/// Body type the proxy hands back to hyper: a boxed stream of bytes whose error
/// is normalised to `std::io::Error`.
type ProxyBody = BoxBody<Bytes, std::io::Error>;

/// What [`ProxyHandle::register_sandbox`] returns: how the guest reaches this sandbox's
/// proxy listener and the token it must present.
#[derive(Debug, Clone)]
pub struct SandboxBinding {
    /// Host-side port the sandbox's listener bound to (reachable from the guest via
    /// the SLIRP/NAT gateway).
    pub port: u16,
    /// Per-sandbox token, hex-encoded, for the guest to present on each connection.
    pub token_hex: String,
}

/// One registered run: its listener task and a shutdown signal.
struct SandboxSlot {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    port: u16,
}

/// Handle to the shared proxy process. Created once and kept warm; sandboxes are
/// registered and unregistered as they start and stop.
pub struct ProxyHandle {
    sandboxes: Arc<Mutex<HashMap<String, SandboxSlot>>>,
    upstream: reqwest::Client,
}

impl ProxyHandle {
    /// Build a handle that re-originates upstream requests with `upstream`.
    /// Exposed so tests can supply a client whose DNS/trust is pointed at a
    /// mock upstream; production uses [`start_proxy`].
    pub fn new(upstream: reqwest::Client) -> Self {
        Self {
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
            upstream,
        }
    }

    /// Register a sandbox: bind a fresh per-sandbox listener, spawn its accept loop, and
    /// return the guest-reachable port + the token to present.
    pub async fn register_sandbox(&self, ctx: SandboxContext) -> Result<SandboxBinding> {
        let token_hex = ctx.token.to_hex();
        let server_config = ctx.ca.server_config();
        let acceptor = TlsAcceptor::from(server_config);

        let listener = TcpListener::bind(guest_accessible_bind_addr(0))
            .await
            .map_err(|e| Error::Network(format!("proxy listener bind failed: {e}")))?;
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
    let presented = req
        .headers()
        .get(PROXY_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(ProxyToken::from_hex);
    let authed = matches!(&presented, Some(token) if ctx.token.matches(token));
    if !authed {
        return text_response(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            "missing or invalid proxy token",
        );
    }

    // --- policy: reachability ---
    if let crate::proxy::Decision::Deny { reason } = ctx.policy.decision(&host) {
        ctx.audit.record(EgressEvent {
            host: host.to_string(),
            port: ctx.upstream_port,
            allowed: false,
            injected: false,
        });
        return text_response(StatusCode::FORBIDDEN, &format!("egress denied: {reason}"));
    }

    let (parts, body) = req.into_parts();
    let mut headers = parts.headers;
    strip_hop_by_hop(&mut headers);
    headers.remove(HeaderName::from_static(PROXY_TOKEN_HEADER));
    headers.remove(HOST);
    // The body is re-originated as an unknown-length stream (chunked), so drop
    // any inbound Content-Length: forwarding it alongside chunked framing is the
    // ambiguity that invites request-smuggling-class bugs.
    headers.remove(CONTENT_LENGTH);

    // --- inject: rewrite the credential header for this exact host ---
    let injected = ctx.injector.inject(&host, &mut headers);

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

/// Whether `name` is a hop-by-hop header that must not be forwarded across the
/// proxy boundary (RFC 7230 §6.1, plus the proxy token).
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
