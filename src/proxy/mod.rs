//! Host-side shared injection proxy — the single egress chokepoint that both
//! the credential track and the (future) egress track plug into.
//!
//! # One shared proxy, not two
//!
//! Credential injection and egress policy are the same concern observed at two
//! granularities: both act on a guest's outbound connection at the host
//! boundary. Running them in one process keeps the memory cost fixed rather
//! than linear in VM count, and lets a single per-connection handler pipeline
//! serve both. The credential [`CredentialInjector`] and the egress
//! `EgressPolicy`/tunnel/audit handlers are *stages in that pipeline*, selected
//! per run via [`RunContext`].
//!
//! # The frozen pipeline contract
//!
//! Every guest connection is processed by a fixed sequence of stages. Later
//! phases extend the proxy by swapping the trait objects carried on
//! [`RunContext`] — **not** by editing the accept/relay loop in [`server`].
//! That separation is the whole point of freezing this interface now: the OAuth
//! store (Phase 1) replaces the [`CredentialInjector`], and real egress
//! policy/tunnelling/audit (Phase 2) replace [`AllowAllPolicy`] and
//! [`DebugAuditSink`], without re-architecting the core.
//!
//! ```text
//! guest conn ─▶ [auth]        per-run token → resolve RunContext (else reject+close)
//!            ─▶ [destination] CONNECT host / TLS SNI / base-URL host
//!            ─▶ [policy]      EgressPolicy::decision        (Phase 0: AllowAll)
//!            ─▶ [injector]    CredentialInjector::inject    (Phase 0: static API key)
//!               (Phase 2 alt: Tunnel — CONNECT pass-through, no TLS-term)
//!            ─▶ [audit]       AuditSink::record             (Phase 0: debug log)
//!            ─▶ upstream      re-resolve per connection, SSRF-pin
//! ```
//!
//! # Process model
//!
//! Phase 0 runs the pipeline as an in-process tokio task (mirroring
//! [`crate::sidecar`]). The pipeline and its untrusted HTTP/TLS parsing are
//! kept behind the [`server`] boundary so a follow-up can move them into a
//! separate low-privilege process without touching callers — the parser surface
//! that motivates that split is contained here.

use std::sync::Arc;

use http::HeaderMap;
use tracing::debug;

pub mod ca;
pub mod injector;
pub mod provision;
pub mod server;
pub mod token;

pub use ca::ProxyCa;
pub use injector::{ApiKeyScheme, StaticApiKeyInjector};
pub use provision::{assert_no_real_credential, build_guest_provisioning, ProxiedUpstream};
pub use server::{start_proxy, ProxyHandle, RunBinding};
pub use token::{ProxyToken, PROXY_TOKEN_HEADER};

/// Outcome of the egress-policy stage for a destination host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The destination may be reached.
    Allow,
    /// The destination is blocked; `reason` is for the audit log.
    Deny { reason: String },
}

impl Decision {
    /// Whether the decision permits the connection.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// One audited egress decision. Phase 0 records the destination and whether a
/// credential was injected; Phase 2 extends this with bytes/timing.
#[derive(Debug, Clone)]
pub struct EgressEvent {
    /// Destination hostname (CONNECT host or TLS SNI).
    pub host: String,
    /// Destination port.
    pub port: u16,
    /// Whether the policy stage allowed the connection.
    pub allowed: bool,
    /// Whether a credential header was injected on this connection.
    pub injected: bool,
}

/// Rewrites the credential header(s) on a request bound for `host` with the
/// host-held secret. Phase 0 ships [`StaticApiKeyInjector`]; Phase 1 swaps in an
/// OAuth-backed implementation that mints a short-lived Bearer per call.
pub trait CredentialInjector: Send + Sync {
    /// Inject the credential for `host` into `headers`, replacing any
    /// guest-supplied placeholder. A no-op for hosts this injector does not own.
    ///
    /// Returns whether a credential was actually injected, so the audit log
    /// records ground truth rather than re-deriving it from the allow-set (the
    /// two can diverge once a run permits more upstreams than the injector owns).
    fn inject(&self, host: &str, headers: &mut HeaderMap) -> bool;
}

/// Decides whether a destination host may be reached. Phase 2 replaces the
/// Phase-0 [`AllowAllPolicy`] with the FQDN allow-list.
pub trait EgressPolicy: Send + Sync {
    /// Decide reachability for `host`.
    fn decision(&self, host: &str) -> Decision;
}

/// Records egress decisions. Phase 2 routes this into `src/observe/`.
pub trait AuditSink: Send + Sync {
    /// Record one egress decision.
    fn record(&self, event: EgressEvent);
}

/// Phase-0 egress policy: allow every destination. Reach is unrestricted until
/// the egress track (Phase 2) lands; credential containment holds regardless.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllPolicy;

impl EgressPolicy for AllowAllPolicy {
    fn decision(&self, _host: &str) -> Decision {
        Decision::Allow
    }
}

/// Phase-0 audit sink: emit each decision at `debug`. Never logs payloads or
/// credentials — only the destination and the allow/inject flags.
#[derive(Debug, Default, Clone, Copy)]
pub struct DebugAuditSink;

impl AuditSink for DebugAuditSink {
    fn record(&self, event: EgressEvent) {
        debug!(
            host = %event.host,
            port = event.port,
            allowed = event.allowed,
            injected = event.injected,
            "proxy egress"
        );
    }
}

/// Per-run state the proxy resolves from a presented [`ProxyToken`]. Holds the
/// pipeline's swappable handler stages plus the run's CA and the set of
/// upstream hostnames the run is permitted to reach (also the CA's name
/// constraints).
#[derive(Clone)]
pub struct RunContext {
    /// Per-run auth token the guest presents on each connection.
    pub token: ProxyToken,
    /// Per-run name-constrained CA used to terminate the guest's TLS.
    pub ca: Arc<ProxyCa>,
    /// Credential-rewrite stage.
    pub injector: Arc<dyn CredentialInjector>,
    /// Reachability stage.
    pub policy: Arc<dyn EgressPolicy>,
    /// Audit stage.
    pub audit: Arc<dyn AuditSink>,
    /// Upstream hosts this run may reach (CA name-constraint + SSRF allow-set).
    pub allowed_upstreams: Vec<String>,
    /// Port to re-originate to on the upstream host. Always 443 in production
    /// (HTTPS providers); overridable so tests can target a local mock.
    pub upstream_port: u16,
}

/// Default upstream port: HTTPS.
pub const DEFAULT_UPSTREAM_PORT: u16 = 443;

impl RunContext {
    /// Build a run context with the Phase-0 default policy (allow-all) and audit
    /// sink (debug log). The CA is name-constrained to `allowed_upstreams`.
    pub fn new(
        token: ProxyToken,
        ca: Arc<ProxyCa>,
        injector: Arc<dyn CredentialInjector>,
        allowed_upstreams: Vec<String>,
    ) -> Self {
        Self {
            token,
            ca,
            injector,
            policy: Arc::new(AllowAllPolicy),
            audit: Arc::new(DebugAuditSink),
            allowed_upstreams,
            upstream_port: DEFAULT_UPSTREAM_PORT,
        }
    }

    /// Override the upstream port (default 443). Intended for tests targeting a
    /// local mock upstream.
    pub fn with_upstream_port(mut self, port: u16) -> Self {
        self.upstream_port = port;
        self
    }

    /// Whether `host` is in this run's permitted upstream set. Used both as the
    /// SSRF allow-set and to mirror the CA's name constraints at request time.
    pub fn permits_upstream(&self, host: &str) -> bool {
        self.allowed_upstreams
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    }
}
