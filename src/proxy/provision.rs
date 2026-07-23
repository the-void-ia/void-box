//! Mapping a provider to the proxy, and the guest-side provisioning that points
//! a client at it without ever staging the real credential.
//!
//! This is the host-side, VM-independent core of provider migration: it decides
//! which providers the RFC-0002 M0 proxy can serve, derives the upstream host +
//! credential scheme, and builds the exact guest env / files / host-aliases that
//! redirect the client through the proxy. The live wiring (owning the proxy
//! handle, writing the CA over the control channel, lifecycle) sits in
//! `agent_box`/`daemon` and is validated by the VM e2e suite; everything here is
//! pure and unit-tested.
//!
//! The [`assert_no_real_credential`] gate is the automated check that a
//! migrated provider leaves no durable secret in the guest's env or files — so a
//! half-migration that both redirects to the proxy *and* leaks the key cannot
//! pass review silently.

use crate::error::{Error, Result};
use crate::llm::LlmProvider;
use crate::proxy::injector::ApiKeyScheme;
use crate::proxy::server::SandboxBinding;
use crate::proxy::PROXY_TOKEN_HEADER;

/// Guest path the per-sandbox CA PEM is written to. Lives under `/home` so it
/// lands in an allowed guest write root — the guest-agent's `fs_guard` permits
/// only `/workspace`, `/home`, and `/etc/voidbox`, and rejects `/tmp`. Referenced
/// by the additive-trust env vars; no `ca-certificates` rebuild.
pub const GUEST_CA_PATH: &str = "/home/sandbox/.voidbox-proxy-ca.pem";

/// Guest path the rendered `/etc/hosts` content is staged to. Under
/// `/etc/voidbox` (an allowed write root), because the guest-agent's `fs_guard`
/// forbids host writes to `/etc/hosts` directly. The guest-agent mirrors this
/// file into `/etc/hosts` on receipt (kept in sync with the guest-agent's
/// `PROXY_HOSTS_CONFIG_PATH`).
pub const GUEST_HOSTS_PATH: &str = "/etc/voidbox/hosts";

/// Render the guest `/etc/hosts`: loopback plus the proxied-upstream → gateway
/// aliases that redirect the client's TLS (SNI = upstream host) onto the
/// per-sandbox proxy listener. Shared by host provisioning and the e2e test so
/// both exercise the same bytes.
pub fn render_guest_hosts(aliases: &[(String, String)]) -> String {
    let mut hosts = String::from("127.0.0.1 localhost\n::1 localhost\n");
    for (ip, host) in aliases {
        hosts.push_str(ip);
        hosts.push(' ');
        hosts.push_str(host);
        hosts.push('\n');
    }
    hosts
}

/// Non-secret placeholder the guest carries in the credential env var. The proxy
/// overwrites it with the real key; some clients require a non-empty value.
pub const ANTHROPIC_KEY_PLACEHOLDER: &str = "voidbox-proxy-placeholder";

/// A provider the M0 proxy (RFC-0002) can serve, with the knobs needed to
/// redirect its client through the proxy.
#[derive(Debug, Clone)]
pub struct ProxiedUpstream {
    /// Upstream host the client talks to (TLS SNI + credential injection target).
    pub host: String,
    /// How the credential is presented on the wire.
    pub scheme: ApiKeyScheme,
    /// Client env var naming the API endpoint (redirected to the proxy).
    pub base_url_env: &'static str,
    /// Client env var for the additive CA-trust PEM path.
    pub ca_env: &'static str,
    /// Client env var that carries arbitrary request headers (used to deliver
    /// the per-sandbox proxy token), if the client supports one.
    pub custom_headers_env: Option<&'static str>,
}

impl ProxiedUpstream {
    /// Map an [`LlmProvider`] to its M0 proxied descriptor, or `None` if the
    /// proxy does not serve the provider yet. M0 serves only Claude.
    pub fn for_provider(provider: &LlmProvider) -> Option<Self> {
        match provider {
            LlmProvider::Claude => Some(Self {
                host: "api.anthropic.com".to_string(),
                scheme: ApiKeyScheme::AnthropicXApiKey,
                base_url_env: "ANTHROPIC_BASE_URL",
                ca_env: "NODE_EXTRA_CA_CERTS",
                custom_headers_env: Some("ANTHROPIC_CUSTOM_HEADERS"),
            }),
            // Custom is deferred to M1: its `env_vars()` already emits a real
            // `ANTHROPIC_BASE_URL`, so redirecting it through the proxy depends on
            // env precedence, and its base URL can carry a path that the proxy's
            // `https://host:port` redirect would drop — both need handling and a
            // VM test before it ships. Codex API-key mode needs `config.toml`
            // redirection, so it lands in M1b with the rest of codex. Local + OAuth
            // providers inject no host-held key here.
            LlmProvider::Custom { .. }
            | LlmProvider::Codex
            | LlmProvider::ClaudePersonal
            | LlmProvider::Ollama { .. }
            | LlmProvider::LmStudio { .. } => None,
        }
    }
}

/// The complete set of guest mutations that redirect a client through the proxy.
#[derive(Debug, Clone)]
pub struct GuestProvisioning {
    /// Env vars to inject into the guest exec environment.
    pub env: Vec<(String, String)>,
    /// `(path, contents)` of the per-sandbox CA PEM to write into the guest.
    pub ca_file: (String, String),
    /// `(ip, host)` aliases to add to the guest's `/etc/hosts` so the upstream
    /// name resolves to the SLIRP/NAT gateway (and thus the proxy listener).
    pub host_aliases: Vec<(String, String)>,
}

/// Build the guest provisioning for `upstream`, given the proxy `binding`, the
/// per-sandbox `ca_pem`, and the guest-visible `gateway_ip`.
pub fn build_guest_provisioning(
    upstream: &ProxiedUpstream,
    binding: &SandboxBinding,
    ca_pem: &str,
    gateway_ip: &str,
) -> GuestProvisioning {
    let base_url = format!("https://{}:{}", upstream.host, binding.port);
    let mut env = vec![
        ("HOME".to_string(), "/home/sandbox".to_string()),
        (upstream.base_url_env.to_string(), base_url),
        (upstream.ca_env.to_string(), GUEST_CA_PATH.to_string()),
    ];
    if matches!(upstream.scheme, ApiKeyScheme::AnthropicXApiKey) {
        // Non-secret placeholder; the proxy injects the real key.
        env.push((
            "ANTHROPIC_API_KEY".to_string(),
            ANTHROPIC_KEY_PLACEHOLDER.to_string(),
        ));
    }
    if let Some(headers_env) = upstream.custom_headers_env {
        env.push((
            headers_env.to_string(),
            format!("{PROXY_TOKEN_HEADER}: {}", binding.token_hex),
        ));
    }

    GuestProvisioning {
        env,
        ca_file: (GUEST_CA_PATH.to_string(), ca_pem.to_string()),
        host_aliases: vec![(gateway_ip.to_string(), upstream.host.clone())],
    }
}

/// Assert no real credential reaches the guest. `secret` is the
/// host-held durable credential; it must not appear in any env value or file
/// contents the sandbox stages into the guest.
pub fn assert_no_real_credential(
    env: &[(String, String)],
    files: &[(String, String)],
    secret: &str,
) -> Result<()> {
    if secret.is_empty() {
        return Ok(());
    }
    if let Some((key, _)) = env.iter().find(|(_, value)| value.contains(secret)) {
        return Err(Error::Network(format!(
            "real credential leaked into guest env var {key}"
        )));
    }
    if files.iter().any(|(_, contents)| contents.contains(secret)) {
        return Err(Error::Network(
            "real credential leaked into a guest file".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ApiKey, LlmProvider};

    fn binding() -> SandboxBinding {
        SandboxBinding {
            port: 54321,
            token_hex: "deadbeef".to_string(),
        }
    }

    #[test]
    fn maps_claude_to_anthropic_upstream() {
        let upstream = ProxiedUpstream::for_provider(&LlmProvider::Claude).expect("claude maps");
        assert_eq!(upstream.host, "api.anthropic.com");
        assert_eq!(upstream.scheme, ApiKeyScheme::AnthropicXApiKey);
    }

    #[test]
    fn only_claude_is_proxied_in_m0() {
        // Custom is deferred to M1 (base-URL precedence + path handling); codex,
        // OAuth, and local providers are out of M0 scope.
        let custom = LlmProvider::Custom {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key: Some(ApiKey::new("sk-secret")),
            model: None,
        };
        assert!(ProxiedUpstream::for_provider(&custom).is_none());
        assert!(ProxiedUpstream::for_provider(&LlmProvider::Codex).is_none());
        assert!(ProxiedUpstream::for_provider(&LlmProvider::ClaudePersonal).is_none());
        assert!(ProxiedUpstream::for_provider(&LlmProvider::ollama("m")).is_none());
    }

    #[test]
    fn provisioning_redirects_without_leaking_secret() {
        let upstream = ProxiedUpstream::for_provider(&LlmProvider::Claude).unwrap();
        let prov = build_guest_provisioning(
            &upstream,
            &binding(),
            "-----BEGIN CERTIFICATE-----",
            "10.0.2.2",
        );

        let base = prov
            .env
            .iter()
            .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(base, "https://api.anthropic.com:54321");

        let key = prov
            .env
            .iter()
            .find(|(k, _)| k == "ANTHROPIC_API_KEY")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(key, ANTHROPIC_KEY_PLACEHOLDER);

        assert_eq!(prov.ca_file.0, GUEST_CA_PATH);
        assert_eq!(
            prov.host_aliases,
            vec![("10.0.2.2".to_string(), "api.anthropic.com".to_string())]
        );

        // The real key appears nowhere.
        assert!(assert_no_real_credential(
            &prov.env,
            std::slice::from_ref(&prov.ca_file),
            "sk-ant-REAL"
        )
        .is_ok());
    }

    #[test]
    fn gate_detects_a_leaked_secret() {
        let env = vec![("ANTHROPIC_API_KEY".to_string(), "sk-ant-REAL".to_string())];
        assert!(assert_no_real_credential(&env, &[], "sk-ant-REAL").is_err());

        let files = vec![("/x".to_string(), "junk sk-ant-REAL junk".to_string())];
        assert!(assert_no_real_credential(&[], &files, "sk-ant-REAL").is_err());
    }
}
