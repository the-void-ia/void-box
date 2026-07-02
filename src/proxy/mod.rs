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
//! per sandbox via [`SandboxContext`].
//!
//! # The frozen pipeline contract
//!
//! Every guest connection is processed by a fixed sequence of stages. Later
//! phases extend the proxy by swapping the trait objects carried on
//! [`SandboxContext`] — **not** by editing the accept/relay loop in [`server`].
//! That separation is the whole point of freezing this interface now: the OAuth
//! store (Phase 1) replaces the [`CredentialInjector`], and real egress
//! policy/tunnelling/audit (Phase 2) replace [`AllowAllPolicy`] and
//! [`DebugAuditSink`], without re-architecting the core.
//!
//! ```text
//! guest conn ─▶ [auth]        per-sandbox token → resolve SandboxContext (else reject+close)
//!            ─▶ [destination] CONNECT host / TLS SNI / base-URL host
//!            ─▶ [policy]      EgressPolicy::decision        (Phase 0: AllowAll)
//!            ─▶ [injector]    CredentialInjector::inject    (Phase 0: static API key)
//!               (Phase 2 alt: Tunnel — CONNECT pass-through, no TLS-term)
//!            ─▶ [audit]       AuditSink::record             (Phase 0: debug log)
//!            ─▶ upstream      resolve per connection, reject internal IPs (SSRF guard)
//! ```
//!
//! # Process model — known deviation from the design
//!
//! The design (ADR-0003, R10) places the proxy in a **separate low-privilege
//! process** from the host runtime: it parses attacker-controlled
//! HTTP/TLS/CONNECT bytes on the host, in the hot path before any auth gate, so a
//! parser compromise must not also be a host-runtime compromise. Phase 0 does
//! **not** do that yet — the pipeline runs as in-process tokio tasks inside the
//! host runtime, so a parser-RCE here is a host-runtime RCE. That is an accepted
//! Phase-0 shortcut, not the designed end state, and it is required hardening
//! before production. The untrusted parsing is kept behind the [`server`]
//! boundary so moving it out-of-process is a deployment change rather than a
//! caller-visible one. The containment invariant — no durable secret in the
//! guest — does not depend on the split; the split bounds blast radius if the
//! parser is exploited.
//!
//! # Snapshot / restore (R11)
//!
//! The design re-mints the per-sandbox CA and token on snapshot restore, because
//! a snapshot that captured guest-held proxy material and reused it verbatim
//! would defeat "per-sandbox ephemeral". A snapshot taken while the proxy is
//! active *does* capture the guest's copy of the token and the CA public cert in
//! guest RAM. What makes that harmless in Phase 0 is the host side: the CA
//! private key and the credential store never enter guest RAM, the cmdline, or
//! `vmm::snapshot` state, and the host listener + token are minted at agent-run
//! time and torn down when the run ends — so a restored guest's captured token
//! has no live listener to present to. The one residual is snapshot-then-clone
//! while the original is still running: the clone's captured token could reach
//! the original's live listener over the shared host-loopback mapping and get a
//! real key injected. That window closes with re-mint-on-restore, which becomes
//! load-bearing once provisioning moves to cold boot (kernel cmdline), in M1.

use std::sync::Arc;

use http::HeaderMap;
use tracing::debug;

pub mod ca;
pub mod injector;
pub mod provision;
pub mod server;
pub mod ssrf;
pub mod token;

pub use ca::ProxyCa;
pub use injector::{ApiKeyScheme, StaticApiKeyInjector};
pub use provision::{
    assert_no_real_credential, build_guest_provisioning, render_guest_hosts, ProxiedUpstream,
    GUEST_HOSTS_PATH,
};
pub use server::{start_proxy, ProxyHandle, SandboxBinding};
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

/// Outcome of the credential-injection stage for one request. Three states,
/// because the caller must fail closed on a failed injection but forward an
/// unowned host — a single boolean cannot tell those apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectOutcome {
    /// A credential header was written for this host.
    Injected,
    /// This injector does not own `host`; no credential applies, and the request
    /// proceeds unchanged.
    NotOwned,
    /// This injector owns `host` but could not produce a valid credential header
    /// (e.g. a malformed key). The caller must reject the request rather than
    /// forward it uncredentialed.
    Failed,
}

/// Rewrites the credential header(s) on a request bound for `host` with the
/// host-held secret. Phase 0 ships [`StaticApiKeyInjector`]; Phase 1 swaps in an
/// OAuth-backed implementation that mints a short-lived Bearer per call.
pub trait CredentialInjector: Send + Sync {
    /// Inject the credential for `host` into `headers`, replacing any
    /// guest-supplied placeholder.
    ///
    /// Returns an [`InjectOutcome`] so the caller can fail closed on a failed
    /// injection and the audit log records ground truth rather than re-deriving
    /// it from the allow-set (the two can diverge once a sandbox permits more
    /// upstreams than the injector owns).
    fn inject(&self, host: &str, headers: &mut HeaderMap) -> InjectOutcome;
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

/// Per-sandbox state the proxy resolves from a presented [`ProxyToken`]. Holds the
/// pipeline's swappable handler stages plus the sandbox's CA and the set of
/// upstream hostnames the sandbox is permitted to reach (also the CA's name
/// constraints).
#[derive(Clone)]
pub struct SandboxContext {
    /// Per-sandbox auth token the guest presents on each connection.
    pub token: ProxyToken,
    /// Per-sandbox name-constrained CA used to terminate the guest's TLS.
    pub ca: Arc<ProxyCa>,
    /// Credential-rewrite stage.
    pub injector: Arc<dyn CredentialInjector>,
    /// Reachability stage.
    pub policy: Arc<dyn EgressPolicy>,
    /// Audit stage.
    pub audit: Arc<dyn AuditSink>,
    /// Upstream hosts this sandbox may reach (CA name-constraint + SSRF allow-set).
    pub allowed_upstreams: Vec<String>,
    /// Port to re-originate to on the upstream host. Always 443 in production
    /// (HTTPS providers); overridable so tests can target a local mock.
    pub upstream_port: u16,
}

/// Default upstream port: HTTPS.
pub const DEFAULT_UPSTREAM_PORT: u16 = 443;

impl SandboxContext {
    /// Build a sandbox context with the Phase-0 default policy (allow-all) and audit
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

    /// Whether `host` is in this sandbox's permitted upstream set. Used both as the
    /// SSRF allow-set and to mirror the CA's name constraints at request time.
    pub fn permits_upstream(&self, host: &str) -> bool {
        self.allowed_upstreams
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    }
}
