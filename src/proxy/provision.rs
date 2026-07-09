//! Mapping a provider to the proxy, and the guest-side provisioning that points
//! a client at it without ever staging the real credential.
//!
//! This is the host-side, VM-independent core of provider migration: it decides
//! which providers the proxy can serve, derives the upstream host + credential
//! scheme, and builds the exact guest env / files / host-aliases that redirect
//! the client through the proxy. The live wiring (owning the proxy handle,
//! writing the CA over the control channel, lifecycle) sits in
//! `agent_box`/`daemon` and is validated by the VM e2e suite; everything here is
//! pure and unit-tested.
//!
//! Two guest clients are provisioned in different ways, selected by
//! [`GuestClient`]. claude-code is env-driven: base URL, CA trust, placeholder
//! credential, and the proxy token all travel as env vars. codex is
//! config-driven: it needs a generated `$CODEX_HOME/config.toml` (a dedicated
//! `voidbox` provider entry, because a config entry cannot override the built-in
//! `openai` provider and the built-in has WebSockets enabled) plus a placeholder
//! `auth.json`; only the CA-trust env (`CODEX_CA_CERTIFICATE`) and `HOME` travel
//! as env vars.
//!
//! The [`assert_no_real_credential`] gate (R14) is the automated check that a
//! migrated provider leaves no durable secret in the guest's env or files — so a
//! half-migration that both redirects to the proxy *and* leaks the key cannot
//! pass review silently.

use std::time::SystemTime;

use base64::Engine;

use crate::credentials::{CodexAuthMode, OAuthProviderKind};
use crate::error::{Error, Result};
use crate::llm::LlmProvider;
use crate::proxy::injector::ApiKeyScheme;
use crate::proxy::server::SandboxBinding;
use crate::proxy::ssrf;
use crate::proxy::{PROXY_TOKEN_BEARER_PREFIX, PROXY_TOKEN_HEADER};

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

/// Guest path of the placeholder codex credential file (`$CODEX_HOME` defaults
/// to `$HOME/.codex`, and the guest exec env sets `HOME=/home/sandbox`).
pub const GUEST_CODEX_AUTH_PATH: &str = "/home/sandbox/.codex/auth.json";

/// Guest path of the generated codex configuration.
pub const GUEST_CODEX_CONFIG_PATH: &str = "/home/sandbox/.codex/config.toml";

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
/// overwrites it with the real key/token; some clients require a non-empty value.
pub const ANTHROPIC_KEY_PLACEHOLDER: &str = "voidbox-proxy-placeholder";

// Client env-var knobs, pinned per guest client. The claude-code trio is honored
// by the bundled claude-code (R9: re-verify on version bumps); the codex CA env
// is honored by the bundled codex (`CODEX_CA_CERTIFICATE`, additive trust,
// verified against codex 0.141.0).
const CLAUDE_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
const CLAUDE_CA_ENV: &str = "NODE_EXTRA_CA_CERTS";
const CLAUDE_CUSTOM_HEADERS_ENV: &str = "ANTHROPIC_CUSTOM_HEADERS";
const CODEX_CA_ENV: &str = "CODEX_CA_CERTIFICATE";

// codex upstream shape, pinned against codex 0.141.0 (R9: re-verify on bumps
// via the codex provisioning harness). API-key mode speaks the OpenAI API
// (`api.openai.com/v1`); ChatGPT mode speaks the ChatGPT codex backend
// (`chatgpt.com/backend-api/codex`). codex appends `/responses` to the base.
const CODEX_UPSTREAM_HOST_API: &str = "api.openai.com";
const CODEX_UPSTREAM_HOST_CHATGPT: &str = "chatgpt.com";
const CODEX_API_BASE_PATH: &str = "/v1";
const CODEX_CHATGPT_BASE_PATH: &str = "/backend-api/codex";

/// Provider-table id of the generated codex provider entry. A dedicated id
/// because codex merges config-file provider entries with `or_insert` — an
/// `[model_providers.openai]` entry cannot override the built-in `openai`
/// provider, and the built-in enables the Responses-over-WebSocket transport the
/// proxy refuses (R8). A fresh id gets user-entry defaults, where
/// `supports_websockets` is off.
const CODEX_PROVIDER_ID: &str = "voidbox";

/// `exp` claim (seconds since epoch) of the placeholder JWTs staged into the
/// guest's codex `auth.json`: 2100-01-01T00:00:00Z. codex proactively refreshes
/// its own tokens when the access-token JWT is within 5 minutes of `exp`, so a
/// far-future expiry keeps the guest codex from ever attempting a refresh with
/// its placeholder (which would fail and could trigger a force-login).
const PLACEHOLDER_JWT_EXP_UNIX: i64 = 4_102_444_800;

/// Default HTTPS port for a proxied upstream.
const HTTPS_PORT: u16 = 443;

/// How the proxy authenticates a proxied upstream, and therefore what placeholder
/// the guest carries in its place.
///
/// A single source of truth for the auth mode rather than overlapping flags: the
/// injector the run builds (static key vs OAuth Bearer) and the placeholder the
/// guest gets both derive from this one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxiedAuth {
    /// Static API key. The guest carries a non-secret placeholder in the key's
    /// place; the proxy injects the host-held key using `scheme`.
    ApiKey(ApiKeyScheme),
    /// Personal-subscription OAuth. The guest carries a placeholder token; the
    /// proxy injects a Bearer minted by the host store for `kind` (Claude
    /// personal, or codex ChatGPT — which also gets the host-held
    /// `chatgpt-account-id`). No credential file with a real secret is staged.
    Oauth(OAuthProviderKind),
}

/// Which guest client is being redirected, and therefore how the proxy
/// coordinates reach it (env-driven claude-code vs config-file-driven codex).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestClient {
    /// claude-code (also serves the Anthropic-compatible Custom provider): base
    /// URL, CA trust, placeholder credential, and the proxy token all travel as
    /// env vars (`ANTHROPIC_BASE_URL`, `NODE_EXTRA_CA_CERTS`,
    /// `ANTHROPIC_CUSTOM_HEADERS`).
    ClaudeCli,
    /// codex: redirected via a generated `config.toml` provider entry plus a
    /// placeholder `auth.json`; only CA trust (`CODEX_CA_CERTIFICATE`) and
    /// `HOME` travel as env vars.
    CodexCli,
}

/// A provider the proxy can serve, with the knobs needed to redirect its client
/// through the proxy.
#[derive(Debug, Clone)]
pub struct ProxiedUpstream {
    /// Upstream host the client talks to (TLS SNI + credential injection target).
    pub host: String,
    /// Upstream port the proxy re-originates to (443 except a Custom provider
    /// whose base URL names another port).
    pub port: u16,
    /// Path prefix of the provider's API base URL (`""` when the base URL is the
    /// bare host). Preserved in the guest's redirected base URL — dropping it
    /// would redirect the client to the wrong API root (e.g. a Custom
    /// `https://openrouter.ai/api/v1`, or codex's `/backend-api/codex`).
    pub base_path: String,
    /// How the credential is authenticated and injected.
    pub auth: ProxiedAuth,
    /// Which guest client is redirected, and by which mechanism.
    pub client: GuestClient,
}

impl ProxiedUpstream {
    /// Map an [`LlmProvider`] to its proxied descriptor.
    ///
    /// `Ok(None)` means the provider takes no host-held credential (local
    /// providers), so there is nothing for the proxy to serve. `Err` is a config
    /// error: a provider the proxy should serve but whose configuration cannot
    /// be redirected safely (a non-HTTPS Custom base URL, an internal-IP Custom
    /// host, or codex without a resolved auth mode).
    ///
    /// `codex_mode` carries the host-side codex auth-mode resolution
    /// ([`crate::credentials::resolve_codex_auth_mode`]); it is only consulted
    /// for [`LlmProvider::Codex`].
    pub fn for_provider(
        provider: &LlmProvider,
        codex_mode: Option<CodexAuthMode>,
    ) -> Result<Option<Self>> {
        match provider {
            LlmProvider::Claude => Ok(Some(Self {
                host: "api.anthropic.com".to_string(),
                port: HTTPS_PORT,
                base_path: String::new(),
                auth: ProxiedAuth::ApiKey(ApiKeyScheme::AnthropicXApiKey),
                client: GuestClient::ClaudeCli,
            })),
            LlmProvider::ClaudePersonal => Ok(Some(Self {
                host: "api.anthropic.com".to_string(),
                port: HTTPS_PORT,
                base_path: String::new(),
                auth: ProxiedAuth::Oauth(OAuthProviderKind::ClaudeCode),
                client: GuestClient::ClaudeCli,
            })),
            LlmProvider::Codex => {
                let mode = codex_mode.ok_or_else(|| {
                    Error::Config(
                        "codex auth mode was not resolved before proxy setup — \
                         run resolve_codex_auth_mode() and pass the result"
                            .into(),
                    )
                })?;
                Ok(Some(match mode {
                    CodexAuthMode::ApiKey => Self {
                        host: CODEX_UPSTREAM_HOST_API.to_string(),
                        port: HTTPS_PORT,
                        base_path: CODEX_API_BASE_PATH.to_string(),
                        auth: ProxiedAuth::ApiKey(ApiKeyScheme::Bearer),
                        client: GuestClient::CodexCli,
                    },
                    CodexAuthMode::ChatGpt => Self {
                        host: CODEX_UPSTREAM_HOST_CHATGPT.to_string(),
                        port: HTTPS_PORT,
                        base_path: CODEX_CHATGPT_BASE_PATH.to_string(),
                        auth: ProxiedAuth::Oauth(OAuthProviderKind::CodexChatGpt),
                        client: GuestClient::CodexCli,
                    },
                }))
            }
            LlmProvider::Custom { base_url, .. } => custom_upstream(base_url).map(Some),
            // Local providers pass only non-secret placeholders; there is no
            // host-held key to contain, so the proxy does not serve them.
            LlmProvider::Ollama { .. } | LlmProvider::LmStudio { .. } => Ok(None),
        }
    }
}

/// Parse a Custom provider's base URL into a proxied descriptor.
///
/// HTTPS-only: the proxy terminates and re-establishes TLS, and the guest's
/// added trust root only affects TLS — a plaintext base URL would put the real
/// key on an unencrypted wire and is refused. The host must also be a name or
/// public IP: the SSRF guard on the upstream client rejects internal ranges at
/// connect time, so an internal-IP base URL is refused here with a clearer
/// error instead of a request-time 502.
fn custom_upstream(base_url: &str) -> Result<ProxiedUpstream> {
    let parsed = url::Url::parse(base_url).map_err(|e| {
        Error::Config(format!(
            "credential_proxy: custom base_url '{base_url}' is not a valid URL: {e}"
        ))
    })?;
    if parsed.scheme() != "https" {
        return Err(Error::Config(format!(
            "credential_proxy: custom base_url '{base_url}' must be https — the proxy \
             injects the real key at TLS egress and will not put it on plaintext HTTP"
        )));
    }
    let host = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.to_string(),
        Some(url::Host::Ipv4(ip)) if !ssrf::is_internal_ip(std::net::IpAddr::V4(ip)) => {
            ip.to_string()
        }
        Some(url::Host::Ipv6(ip)) if !ssrf::is_internal_ip(std::net::IpAddr::V6(ip)) => {
            ip.to_string()
        }
        Some(_) => {
            return Err(Error::Config(format!(
                "credential_proxy: custom base_url '{base_url}' targets an internal \
                 address; the proxy's SSRF guard would refuse it at request time \
                 (internal endpoints do not need the credential proxy)"
            )));
        }
        None => {
            return Err(Error::Config(format!(
                "credential_proxy: custom base_url '{base_url}' has no host"
            )));
        }
    };
    let base_path = match parsed.path() {
        "/" => String::new(),
        path => path.trim_end_matches('/').to_string(),
    };
    Ok(ProxiedUpstream {
        host,
        port: parsed.port().unwrap_or(HTTPS_PORT),
        base_path,
        // The Anthropic-compatible Custom provider shares Claude's client and
        // credential header (`x-api-key`).
        auth: ProxiedAuth::ApiKey(ApiKeyScheme::AnthropicXApiKey),
        client: GuestClient::ClaudeCli,
    })
}

/// The complete set of guest mutations that redirect a client through the proxy.
#[derive(Debug, Clone)]
pub struct GuestProvisioning {
    /// Env vars to inject into the guest exec environment.
    pub env: Vec<(String, String)>,
    /// `(path, contents)` files to write into the guest: always the per-sandbox
    /// CA PEM, plus the placeholder `auth.json` for a codex client. The codex
    /// `config.toml` is not here — it is composed by the caller (via
    /// [`render_codex_config_toml`]) because MCP server entries share that file.
    pub files: Vec<(String, String)>,
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
    let base_url = format!(
        "https://{}:{}{}",
        upstream.host, binding.port, upstream.base_path
    );
    let mut env = vec![("HOME".to_string(), "/home/sandbox".to_string())];
    let mut files = vec![(GUEST_CA_PATH.to_string(), ca_pem.to_string())];

    match upstream.client {
        GuestClient::ClaudeCli => {
            env.push((CLAUDE_BASE_URL_ENV.to_string(), base_url));
            env.push((CLAUDE_CA_ENV.to_string(), GUEST_CA_PATH.to_string()));
            match upstream.auth {
                // Non-secret placeholder; the proxy injects the real key.
                ProxiedAuth::ApiKey(ApiKeyScheme::AnthropicXApiKey) => {
                    env.push((
                        "ANTHROPIC_API_KEY".to_string(),
                        ANTHROPIC_KEY_PLACEHOLDER.to_string(),
                    ));
                }
                // No claude-code-shaped provider uses a Bearer API key; nothing
                // to stage if one ever does — the proxy injects regardless.
                ProxiedAuth::ApiKey(ApiKeyScheme::Bearer) => {}
                // Personal OAuth: a placeholder auth token the proxy replaces
                // with a host-minted Bearer, plus the host-managed-provider flag
                // that suppresses the client's OAuth-refresh recovery and
                // force-login. No credential file.
                ProxiedAuth::Oauth(_) => {
                    env.push((
                        "ANTHROPIC_AUTH_TOKEN".to_string(),
                        ANTHROPIC_KEY_PLACEHOLDER.to_string(),
                    ));
                    env.push((
                        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST".to_string(),
                        "1".to_string(),
                    ));
                }
            }
            env.push((
                CLAUDE_CUSTOM_HEADERS_ENV.to_string(),
                format!("{PROXY_TOKEN_HEADER}: {}", binding.token_hex),
            ));
        }
        GuestClient::CodexCli => {
            // codex's base-URL redirect and proxy token travel in config.toml
            // (rendered by the caller alongside MCP entries); the placeholder
            // auth.json is staged here, and only CA trust is env-driven.
            env.push((CODEX_CA_ENV.to_string(), GUEST_CA_PATH.to_string()));
            let mode = match upstream.auth {
                ProxiedAuth::ApiKey(_) => CodexAuthMode::ApiKey,
                ProxiedAuth::Oauth(_) => CodexAuthMode::ChatGpt,
            };
            files.push((
                GUEST_CODEX_AUTH_PATH.to_string(),
                render_codex_auth_json(mode, &binding.token_hex),
            ));
        }
    }

    GuestProvisioning {
        env,
        files,
        host_aliases: vec![(gateway_ip.to_string(), upstream.host.clone())],
    }
}

/// Render a syntactically valid unsigned JWT whose payload carries only the
/// far-future [`PLACEHOLDER_JWT_EXP_UNIX`] expiry. codex requires `id_token` to
/// parse as a JWT and reads the access token's `exp` to schedule its own
/// refresh; this placeholder satisfies the parser while making that refresh
/// never fire.
fn placeholder_jwt() -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = b64.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = b64.encode(format!(r#"{{"exp":{PLACEHOLDER_JWT_EXP_UNIX}}}"#));
    format!("{header}.{payload}.voidbox-placeholder")
}

/// Render the placeholder `auth.json` staged into the guest for a proxied codex
/// run. Carries no secret: the ChatGPT-mode tokens are dummy far-future JWTs and
/// filler strings; the API-key-mode "key" is the token-bearing placeholder
/// (`voidbox-proxy-<token_hex>`), which codex sends as `Authorization: Bearer` —
/// that is how the per-sandbox proxy token reaches the proxy on this path, and
/// the proxy strips or replaces it before anything goes upstream.
///
/// `last_refresh` is stamped now so codex's staleness fallback (refresh when the
/// file is older than its refresh interval) also never fires.
pub fn render_codex_auth_json(mode: CodexAuthMode, token_hex: &str) -> String {
    let document = match mode {
        CodexAuthMode::ChatGpt => serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": placeholder_jwt(),
                "access_token": placeholder_jwt(),
                "refresh_token": ANTHROPIC_KEY_PLACEHOLDER,
                "account_id": ANTHROPIC_KEY_PLACEHOLDER,
            },
            "last_refresh": humantime::format_rfc3339(SystemTime::now()).to_string(),
        }),
        CodexAuthMode::ApiKey => serde_json::json!({
            "auth_mode": "apikey",
            "OPENAI_API_KEY": format!("{PROXY_TOKEN_BEARER_PREFIX}{token_hex}"),
        }),
    };
    serde_json::to_string_pretty(&document).expect("static JSON document always serializes")
}

/// Render the codex MCP-server discovery section of `config.toml` (one
/// `[mcp_servers."name"]` table per HTTP server). Shared by the proxied path
/// (where it is composed into [`render_codex_config_toml`]) and the unproxied
/// path (where it is the whole file).
pub fn render_codex_mcp_servers_toml(servers: &[(String, String)]) -> String {
    let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let mut toml = String::new();
    for (name, server_url) in servers {
        toml.push_str(&format!(
            "[mcp_servers.\"{}\"]\nurl = \"{}\"\n\n",
            escape(name),
            escape(server_url)
        ));
    }
    toml
}

/// Render the complete generated codex `config.toml` for a proxied run: the
/// dedicated `voidbox` provider entry redirecting codex at the proxy, followed
/// by any MCP-server section (`mcp_servers_toml`, pre-rendered by
/// [`render_codex_mcp_servers_toml`]; empty when the run has no MCP servers).
///
/// Every key is pinned against codex 0.141.0 (R9: the codex provisioning
/// harness re-verifies on version bumps):
/// - `model_provider` selects the generated entry; `cli_auth_credentials_store =
///   "file"` pins auth to the staged `auth.json` (away from any keyring).
/// - `name = "OpenAI"` keeps codex's OpenAI-specific request shape (the
///   `chatgpt-account-id`/`originator` headers ride on it).
/// - `requires_openai_auth = true` makes codex authenticate from `auth.json`.
/// - `wire_api = "responses"` is the only supported wire API.
/// - `supports_websockets = false` forces plain HTTPS (R8): the proxy cannot
///   inject into a WebSocket upgrade and refuses one with a 502.
/// - the `http_headers` table carries the per-sandbox proxy token on every
///   request, mirroring claude-code's `ANTHROPIC_CUSTOM_HEADERS`.
pub fn render_codex_config_toml(
    upstream: &ProxiedUpstream,
    binding: &SandboxBinding,
    mcp_servers_toml: &str,
) -> String {
    let base_url = format!(
        "https://{}:{}{}",
        upstream.host, binding.port, upstream.base_path
    );
    let mut toml = format!(
        r#"# Generated by void-box: routes codex through the per-sandbox credential
# proxy. Do not edit; regenerated on every run.
model_provider = "{CODEX_PROVIDER_ID}"
cli_auth_credentials_store = "file"

[model_providers.{CODEX_PROVIDER_ID}]
name = "OpenAI"
base_url = "{base_url}"
requires_openai_auth = true
wire_api = "responses"
supports_websockets = false

[model_providers.{CODEX_PROVIDER_ID}.http_headers]
"{PROXY_TOKEN_HEADER}" = "{token_hex}"
"#,
        token_hex = binding.token_hex,
    );
    if !mcp_servers_toml.is_empty() {
        toml.push('\n');
        toml.push_str(mcp_servers_toml);
    }
    toml
}

/// R14 gate: assert no real credential reaches the guest. `secret` is the
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
            "R14: real credential leaked into guest env var {key}"
        )));
    }
    if files.iter().any(|(_, contents)| contents.contains(secret)) {
        return Err(Error::Network(
            "R14: real credential leaked into a guest file".to_string(),
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

    fn env_value<'a>(prov: &'a GuestProvisioning, key: &str) -> Option<&'a str> {
        prov.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn file_contents<'a>(prov: &'a GuestProvisioning, path: &str) -> Option<&'a str> {
        prov.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.as_str())
    }

    #[test]
    fn maps_claude_to_anthropic_api_key_upstream() {
        let upstream = ProxiedUpstream::for_provider(&LlmProvider::Claude, None)
            .expect("claude maps")
            .expect("served");
        assert_eq!(upstream.host, "api.anthropic.com");
        assert_eq!(upstream.port, 443);
        assert_eq!(upstream.base_path, "");
        assert_eq!(
            upstream.auth,
            ProxiedAuth::ApiKey(ApiKeyScheme::AnthropicXApiKey)
        );
        assert_eq!(upstream.client, GuestClient::ClaudeCli);
    }

    #[test]
    fn maps_claude_personal_to_anthropic_oauth_upstream() {
        let upstream = ProxiedUpstream::for_provider(&LlmProvider::ClaudePersonal, None)
            .expect("claude-personal maps")
            .expect("served");
        assert_eq!(upstream.host, "api.anthropic.com");
        assert_eq!(
            upstream.auth,
            ProxiedAuth::Oauth(OAuthProviderKind::ClaudeCode)
        );
    }

    #[test]
    fn maps_codex_per_auth_mode() {
        let api = ProxiedUpstream::for_provider(&LlmProvider::Codex, Some(CodexAuthMode::ApiKey))
            .expect("codex maps")
            .expect("served");
        assert_eq!(api.host, "api.openai.com");
        assert_eq!(api.port, 443);
        assert_eq!(api.base_path, "/v1");
        assert_eq!(api.auth, ProxiedAuth::ApiKey(ApiKeyScheme::Bearer));
        assert_eq!(api.client, GuestClient::CodexCli);

        let chatgpt =
            ProxiedUpstream::for_provider(&LlmProvider::Codex, Some(CodexAuthMode::ChatGpt))
                .expect("codex maps")
                .expect("served");
        assert_eq!(chatgpt.host, "chatgpt.com");
        assert_eq!(chatgpt.base_path, "/backend-api/codex");
        assert_eq!(
            chatgpt.auth,
            ProxiedAuth::Oauth(OAuthProviderKind::CodexChatGpt)
        );
        assert_eq!(chatgpt.client, GuestClient::CodexCli);

        // codex without a resolved mode is a config error, not a guess.
        assert!(ProxiedUpstream::for_provider(&LlmProvider::Codex, None).is_err());
    }

    #[test]
    fn maps_custom_preserving_path_and_port() {
        let custom = LlmProvider::Custom {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key: Some(ApiKey::new("sk-secret")),
            model: None,
        };
        let upstream = ProxiedUpstream::for_provider(&custom, None)
            .expect("custom maps")
            .expect("served");
        assert_eq!(upstream.host, "openrouter.ai");
        assert_eq!(upstream.port, 443);
        assert_eq!(upstream.base_path, "/api/v1");
        assert_eq!(
            upstream.auth,
            ProxiedAuth::ApiKey(ApiKeyScheme::AnthropicXApiKey)
        );
        assert_eq!(upstream.client, GuestClient::ClaudeCli);

        let with_port = LlmProvider::Custom {
            base_url: "https://gateway.example.com:8443/anthropic/".to_string(),
            api_key: Some(ApiKey::new("sk-secret")),
            model: None,
        };
        let upstream = ProxiedUpstream::for_provider(&with_port, None)
            .unwrap()
            .unwrap();
        assert_eq!(upstream.port, 8443);
        // Trailing slash trimmed so the client's appended path never doubles it.
        assert_eq!(upstream.base_path, "/anthropic");
    }

    #[test]
    fn custom_rejects_http_and_internal_hosts() {
        let http = LlmProvider::Custom {
            base_url: "http://gateway.example.com/v1".to_string(),
            api_key: Some(ApiKey::new("sk-secret")),
            model: None,
        };
        let err = ProxiedUpstream::for_provider(&http, None).unwrap_err();
        assert!(err.to_string().contains("https"));

        let internal = LlmProvider::Custom {
            base_url: "https://10.0.2.2:11434/v1".to_string(),
            api_key: Some(ApiKey::new("sk-secret")),
            model: None,
        };
        let err = ProxiedUpstream::for_provider(&internal, None).unwrap_err();
        assert!(err.to_string().contains("internal"));
    }

    #[test]
    fn local_providers_are_not_proxied() {
        assert!(
            ProxiedUpstream::for_provider(&LlmProvider::ollama("m"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn provisioning_redirects_without_leaking_secret() {
        let upstream = ProxiedUpstream::for_provider(&LlmProvider::Claude, None)
            .unwrap()
            .unwrap();
        let prov = build_guest_provisioning(
            &upstream,
            &binding(),
            "-----BEGIN CERTIFICATE-----",
            "10.0.2.2",
        );

        assert_eq!(
            env_value(&prov, "ANTHROPIC_BASE_URL"),
            Some("https://api.anthropic.com:54321")
        );
        assert_eq!(
            env_value(&prov, "ANTHROPIC_API_KEY"),
            Some(ANTHROPIC_KEY_PLACEHOLDER)
        );
        assert_eq!(
            file_contents(&prov, GUEST_CA_PATH),
            Some("-----BEGIN CERTIFICATE-----")
        );
        assert_eq!(
            prov.host_aliases,
            vec![("10.0.2.2".to_string(), "api.anthropic.com".to_string())]
        );

        // R14: the real key appears nowhere.
        assert!(assert_no_real_credential(&prov.env, &prov.files, "sk-ant-REAL").is_ok());
    }

    #[test]
    fn claude_personal_provisions_oauth_placeholder_without_api_key() {
        let upstream = ProxiedUpstream::for_provider(&LlmProvider::ClaudePersonal, None)
            .unwrap()
            .unwrap();
        let prov = build_guest_provisioning(
            &upstream,
            &binding(),
            "-----BEGIN CERTIFICATE-----",
            "10.0.2.2",
        );

        assert_eq!(
            env_value(&prov, "ANTHROPIC_BASE_URL"),
            Some("https://api.anthropic.com:54321")
        );
        // OAuth carries a placeholder auth *token*, not an API key, plus the
        // host-managed-provider flag; no `ANTHROPIC_API_KEY` is set.
        assert_eq!(
            env_value(&prov, "ANTHROPIC_AUTH_TOKEN"),
            Some(ANTHROPIC_KEY_PLACEHOLDER)
        );
        assert_eq!(
            env_value(&prov, "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"),
            Some("1")
        );
        assert_eq!(env_value(&prov, "ANTHROPIC_API_KEY"), None);

        // R14: the durable refresh token appears nowhere in the staged env/files.
        assert!(
            assert_no_real_credential(&prov.env, &prov.files, "real-refresh-token-value").is_ok()
        );
    }

    #[test]
    fn custom_provisioning_preserves_base_path_in_redirect() {
        let custom = LlmProvider::Custom {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key: Some(ApiKey::new("sk-custom-REAL")),
            model: None,
        };
        let upstream = ProxiedUpstream::for_provider(&custom, None)
            .unwrap()
            .unwrap();
        let prov = build_guest_provisioning(&upstream, &binding(), "PEM", "10.0.2.2");
        assert_eq!(
            env_value(&prov, "ANTHROPIC_BASE_URL"),
            Some("https://openrouter.ai:54321/api/v1")
        );
        assert_eq!(
            env_value(&prov, "ANTHROPIC_API_KEY"),
            Some(ANTHROPIC_KEY_PLACEHOLDER)
        );
        assert_eq!(
            prov.host_aliases,
            vec![("10.0.2.2".to_string(), "openrouter.ai".to_string())]
        );
        assert!(assert_no_real_credential(&prov.env, &prov.files, "sk-custom-REAL").is_ok());
    }

    #[test]
    fn codex_api_key_provisioning_stages_token_bearing_auth_json() {
        let upstream =
            ProxiedUpstream::for_provider(&LlmProvider::Codex, Some(CodexAuthMode::ApiKey))
                .unwrap()
                .unwrap();
        let prov = build_guest_provisioning(&upstream, &binding(), "PEM", "10.0.2.2");

        // Env carries only HOME + CA trust; the redirect lives in config.toml.
        assert_eq!(
            env_value(&prov, "CODEX_CA_CERTIFICATE"),
            Some(GUEST_CA_PATH)
        );
        assert_eq!(env_value(&prov, "ANTHROPIC_BASE_URL"), None);
        assert_eq!(env_value(&prov, "OPENAI_API_KEY"), None);

        let auth_json = file_contents(&prov, GUEST_CODEX_AUTH_PATH).expect("auth.json staged");
        let parsed: serde_json::Value = serde_json::from_str(auth_json).expect("valid JSON");
        assert_eq!(parsed["auth_mode"], "apikey");
        // The placeholder key carries the per-sandbox token so codex's
        // `Authorization: Bearer` presents it to the proxy.
        assert_eq!(parsed["OPENAI_API_KEY"], "voidbox-proxy-deadbeef");

        assert_eq!(
            prov.host_aliases,
            vec![("10.0.2.2".to_string(), "api.openai.com".to_string())]
        );
        assert!(assert_no_real_credential(&prov.env, &prov.files, "sk-openai-REAL").is_ok());
    }

    #[test]
    fn codex_chatgpt_provisioning_stages_placeholder_jwts() {
        let upstream =
            ProxiedUpstream::for_provider(&LlmProvider::Codex, Some(CodexAuthMode::ChatGpt))
                .unwrap()
                .unwrap();
        let prov = build_guest_provisioning(&upstream, &binding(), "PEM", "10.0.2.2");

        let auth_json = file_contents(&prov, GUEST_CODEX_AUTH_PATH).expect("auth.json staged");
        let parsed: serde_json::Value = serde_json::from_str(auth_json).expect("valid JSON");
        assert_eq!(parsed["auth_mode"], "chatgpt");
        assert_eq!(parsed["OPENAI_API_KEY"], serde_json::Value::Null);
        assert_eq!(parsed["tokens"]["refresh_token"], ANTHROPIC_KEY_PLACEHOLDER);
        assert_eq!(parsed["tokens"]["account_id"], ANTHROPIC_KEY_PLACEHOLDER);

        // Both JWTs are well-formed three-segment tokens whose payload decodes
        // to the far-future exp — codex's parser accepts them and its proactive
        // refresh never fires.
        for field in ["id_token", "access_token"] {
            let jwt = parsed["tokens"][field].as_str().expect("jwt string");
            let segments: Vec<&str> = jwt.split('.').collect();
            assert_eq!(segments.len(), 3, "{field} must be a 3-segment JWT");
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(segments[1])
                .expect("base64url payload");
            let claims: serde_json::Value = serde_json::from_slice(&payload).expect("JSON claims");
            assert_eq!(claims["exp"], PLACEHOLDER_JWT_EXP_UNIX);
        }
        assert!(humantime::parse_rfc3339(parsed["last_refresh"].as_str().unwrap()).is_ok());

        // R14 over a dummy durable secret.
        assert!(assert_no_real_credential(&prov.env, &prov.files, "real-refresh-token").is_ok());
    }

    #[test]
    fn codex_config_toml_pins_provider_entry_and_token() {
        let upstream =
            ProxiedUpstream::for_provider(&LlmProvider::Codex, Some(CodexAuthMode::ChatGpt))
                .unwrap()
                .unwrap();
        let toml = render_codex_config_toml(&upstream, &binding(), "");

        assert!(toml.contains("model_provider = \"voidbox\""));
        assert!(toml.contains("cli_auth_credentials_store = \"file\""));
        assert!(toml.contains("[model_providers.voidbox]"));
        assert!(toml.contains("name = \"OpenAI\""));
        assert!(toml.contains("base_url = \"https://chatgpt.com:54321/backend-api/codex\""));
        assert!(toml.contains("requires_openai_auth = true"));
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(toml.contains("supports_websockets = false"));
        assert!(toml.contains("[model_providers.voidbox.http_headers]"));
        assert!(toml.contains("\"x-voidbox-proxy-token\" = \"deadbeef\""));
    }

    #[test]
    fn codex_config_toml_composes_mcp_servers() {
        let upstream =
            ProxiedUpstream::for_provider(&LlmProvider::Codex, Some(CodexAuthMode::ApiKey))
                .unwrap()
                .unwrap();
        let mcp = render_codex_mcp_servers_toml(&[(
            "void-mcp".to_string(),
            "http://127.0.0.1:8222/mcp".to_string(),
        )]);
        let toml = render_codex_config_toml(&upstream, &binding(), &mcp);
        assert!(toml.contains("base_url = \"https://api.openai.com:54321/v1\""));
        assert!(toml.contains("[mcp_servers.\"void-mcp\"]"));
        assert!(toml.contains("url = \"http://127.0.0.1:8222/mcp\""));
    }

    #[test]
    fn mcp_servers_toml_escapes_quotes_and_backslashes() {
        let toml = render_codex_mcp_servers_toml(&[(
            r#"we"ird\name"#.to_string(),
            "http://x/".to_string(),
        )]);
        assert!(toml.contains(r#"[mcp_servers."we\"ird\\name"]"#));
    }

    #[test]
    fn r14_detects_a_leaked_secret() {
        let env = vec![("ANTHROPIC_API_KEY".to_string(), "sk-ant-REAL".to_string())];
        assert!(assert_no_real_credential(&env, &[], "sk-ant-REAL").is_err());

        let files = vec![("/x".to_string(), "junk sk-ant-REAL junk".to_string())];
        assert!(assert_no_real_credential(&[], &files, "sk-ant-REAL").is_err());
    }
}
