//! The two [`CredentialInjector`] implementations, selected per provider and
//! auth mode behind the same trait boundary.
//!
//! [`StaticApiKeyInjector`] holds one host-held API key already in memory and
//! rewrites the credential header for the exact upstream host it owns. It serves
//! the API-key providers (Claude with an API key, the Anthropic-compatible Custom
//! provider, and codex API-key mode) — the sanctioned path for programmatic use.
//!
//! [`OAuthBearerInjector`] serves personal-subscription OAuth (Claude personal
//! and codex ChatGPT): it asks the host [`OAuthTokenStore`] for a currently-valid
//! access token per request and writes it as a Bearer, so the durable refresh
//! token stays on the host and never reaches the guest. codex additionally
//! carries the host-held account identity (`chatgpt-account-id`) as an extra
//! static header on the same injector, replacing the guest's placeholder.
//!
//! Both match their upstream host **exactly** (R3): an injector never attaches a
//! secret to a request whose host differs from the one it was configured for, so
//! an agent-controlled `Host` header (which the proxy strips anyway) or a
//! misrouted connection cannot redirect the credential to another destination.
//! Path-scoped injection (crediting only specific paths on a host, needed once
//! non-LLM downstream services share this trait) is deferred to M2; both here
//! credit every path on their single name-constrained LLM upstream.

use std::sync::Arc;

use async_trait::async_trait;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use tracing::warn;

use crate::credentials::OAuthTokenStore;
use crate::proxy::{CredentialInjector, InjectOutcome};

/// Anthropic credential header.
const ANTHROPIC_API_KEY_HEADER: &str = "x-api-key";
/// Bearer credential header.
const AUTHORIZATION_HEADER: &str = "authorization";
/// ChatGPT account-identity header codex sends alongside its Bearer; the proxy
/// replaces the guest's placeholder with the host-held account id.
pub const CHATGPT_ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";

/// How a provider expects its API key presented on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeyScheme {
    /// Anthropic-style `x-api-key: <key>` (Claude and the Anthropic-compatible
    /// Custom provider).
    AnthropicXApiKey,
    /// `Authorization: Bearer <key>` (OpenAI / codex API-key mode).
    Bearer,
}

/// Injects a single static API key for one owned upstream host.
pub struct StaticApiKeyInjector {
    /// The exact upstream host this injector owns (case-insensitive match).
    host: String,
    /// How to present the key.
    scheme: ApiKeyScheme,
    /// The host-held secret. Never logged; lives only in host memory.
    api_key: SecretString,
}

impl StaticApiKeyInjector {
    /// Build an injector that attaches `api_key` to requests for `host` using
    /// `scheme`.
    pub fn new(host: impl Into<String>, scheme: ApiKeyScheme, api_key: SecretString) -> Self {
        Self {
            host: host.into(),
            scheme,
            api_key,
        }
    }
}

/// Set `name: value`, replacing any existing (guest-supplied placeholder) value,
/// and mark the value sensitive so it is excluded from header debug output.
/// Returns `false` for a malformed value (non-visible-ASCII secret): the header
/// is removed rather than forwarded, so a bad key can never leak as a literal
/// header, and the caller fails the request closed rather than sending it
/// uncredentialed.
fn set_secret_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> bool {
    match HeaderValue::from_str(value) {
        Ok(mut header_value) => {
            header_value.set_sensitive(true);
            headers.insert(HeaderName::from_static(name), header_value);
            true
        }
        Err(_) => {
            warn!(
                header = name,
                "dropping credential header: value not valid ASCII"
            );
            headers.remove(name);
            false
        }
    }
}

#[async_trait]
impl CredentialInjector for StaticApiKeyInjector {
    async fn inject(&self, host: &str, headers: &mut HeaderMap) -> InjectOutcome {
        if !self.host.eq_ignore_ascii_case(host) {
            return InjectOutcome::NotOwned;
        }
        let secret = self.api_key.expose_secret();
        let ok = match self.scheme {
            ApiKeyScheme::AnthropicXApiKey => {
                headers.remove(AUTHORIZATION_HEADER);
                set_secret_header(headers, ANTHROPIC_API_KEY_HEADER, secret)
            }
            ApiKeyScheme::Bearer => {
                headers.remove(ANTHROPIC_API_KEY_HEADER);
                set_secret_header(headers, AUTHORIZATION_HEADER, &format!("Bearer {secret}"))
            }
        };
        if ok {
            InjectOutcome::Injected
        } else {
            InjectOutcome::Failed
        }
    }
}

/// Injects an OAuth access token as `Authorization: Bearer`, minted per request
/// from the host [`OAuthTokenStore`], for the single upstream host it owns.
///
/// The durable refresh token stays in the store (host memory); the guest holds
/// only a placeholder. Awaiting the store here (rather than caching a token on
/// the injector) is what makes the store the single serialized rotation owner:
/// concurrent requests share one refresh instead of each spending the single-use
/// refresh token.
pub struct OAuthBearerInjector {
    /// The exact upstream host this injector owns (case-insensitive match).
    host: String,
    /// Host-side custodian that mints short-lived access tokens.
    store: Arc<OAuthTokenStore>,
    /// Host-held static headers written alongside the Bearer (e.g. the codex
    /// `chatgpt-account-id`), replacing any guest-supplied placeholder value.
    /// Removed together with the credential headers on a failed injection.
    extra_headers: Vec<(&'static str, String)>,
}

impl OAuthBearerInjector {
    /// Build an injector that credits requests for `host` with a Bearer minted
    /// by `store`.
    pub fn new(host: impl Into<String>, store: Arc<OAuthTokenStore>) -> Self {
        Self {
            host: host.into(),
            store,
            extra_headers: Vec::new(),
        }
    }

    /// Also write the host-held static header `name: value` on every injected
    /// request, replacing the guest's placeholder. Used for identity headers the
    /// provider requires next to the Bearer (codex's `chatgpt-account-id`).
    pub fn with_extra_header(mut self, name: &'static str, value: String) -> Self {
        self.extra_headers.push((name, value));
        self
    }

    /// Drop every header this injector owns, so a failed injection can never
    /// leave a partial credential (or a stale placeholder identity) behind.
    fn drop_owned_headers(&self, headers: &mut HeaderMap) {
        headers.remove(ANTHROPIC_API_KEY_HEADER);
        headers.remove(AUTHORIZATION_HEADER);
        for (name, _) in &self.extra_headers {
            headers.remove(*name);
        }
    }
}

#[async_trait]
impl CredentialInjector for OAuthBearerInjector {
    async fn inject(&self, host: &str, headers: &mut HeaderMap) -> InjectOutcome {
        if !self.host.eq_ignore_ascii_case(host) {
            return InjectOutcome::NotOwned;
        }
        // Fail closed on any store error (expired token with a failed/rate-capped
        // refresh, or an unavailable store): drop every credential header so the
        // request can never be forwarded uncredentialed, and let the caller
        // return a 502 rather than send an unauthenticated upstream call.
        let token = match self.store.access_token().await {
            Ok(token) => token,
            Err(e) => {
                warn!(host = %host, "OAuth injector: no valid access token ({e}); failing closed");
                self.drop_owned_headers(headers);
                return InjectOutcome::Failed;
            }
        };
        headers.remove(ANTHROPIC_API_KEY_HEADER);
        let mut ok = set_secret_header(
            headers,
            AUTHORIZATION_HEADER,
            &format!("Bearer {}", token.expose_secret()),
        );
        for (name, value) in &self.extra_headers {
            ok = ok && set_secret_header(headers, name, value.as_str());
        }
        if ok {
            InjectOutcome::Injected
        } else {
            self.drop_owned_headers(headers);
            InjectOutcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::credentials::OAuthProviderKind;

    fn placeholder_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("placeholder-not-a-real-key"),
        );
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer placeholder"),
        );
        headers
    }

    #[tokio::test]
    async fn anthropic_injects_x_api_key_and_drops_bearer() {
        let injector = StaticApiKeyInjector::new(
            "api.anthropic.com",
            ApiKeyScheme::AnthropicXApiKey,
            SecretString::from("sk-real-secret"),
        );
        let mut headers = placeholder_headers();
        assert_eq!(
            injector.inject("api.anthropic.com", &mut headers).await,
            InjectOutcome::Injected
        );

        assert_eq!(headers.get("x-api-key").unwrap(), "sk-real-secret");
        assert!(headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn bearer_injects_authorization_and_drops_x_api_key() {
        let injector = StaticApiKeyInjector::new(
            "api.openai.com",
            ApiKeyScheme::Bearer,
            SecretString::from("sk-openai-secret"),
        );
        let mut headers = placeholder_headers();
        assert_eq!(
            injector.inject("api.openai.com", &mut headers).await,
            InjectOutcome::Injected
        );

        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer sk-openai-secret"
        );
        assert!(headers.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn does_not_inject_for_other_hosts() {
        let injector = StaticApiKeyInjector::new(
            "api.anthropic.com",
            ApiKeyScheme::AnthropicXApiKey,
            SecretString::from("sk-real-secret"),
        );
        let mut headers = HeaderMap::new();
        assert_eq!(
            injector.inject("evil.example.com", &mut headers).await,
            InjectOutcome::NotOwned
        );
        assert!(headers.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn injected_value_is_marked_sensitive() {
        let injector = StaticApiKeyInjector::new(
            "api.anthropic.com",
            ApiKeyScheme::AnthropicXApiKey,
            SecretString::from("sk-real-secret"),
        );
        let mut headers = HeaderMap::new();
        injector.inject("api.anthropic.com", &mut headers).await;
        assert!(headers.get("x-api-key").unwrap().is_sensitive());
    }

    #[tokio::test]
    async fn malformed_key_for_owned_host_fails_and_drops_header() {
        // A key with a non-visible-ASCII byte cannot become a header value. The
        // injector must report `Failed` (so the caller fails closed) and leave no
        // credential header behind — never forward the placeholder or a partial.
        let injector = StaticApiKeyInjector::new(
            "api.anthropic.com",
            ApiKeyScheme::AnthropicXApiKey,
            SecretString::from("sk-bad\nkey"),
        );
        let mut headers = placeholder_headers();
        assert_eq!(
            injector.inject("api.anthropic.com", &mut headers).await,
            InjectOutcome::Failed
        );
        assert!(
            headers.get("x-api-key").is_none(),
            "a malformed key must not be forwarded as a header"
        );
    }

    // ---- OAuth injector ----

    fn ms_from_now(delta_secs: i64) -> i64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        now + delta_secs * 1000
    }

    fn creds_json(access: &str, refresh: &str, expires_at_ms: i64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":{expires_at_ms}}}}}"#
        )
    }

    /// A store holding a far-future access token — no refresh, no network.
    fn store_with_valid_token(access: &str) -> Arc<OAuthTokenStore> {
        Arc::new(
            OAuthTokenStore::from_json(
                OAuthProviderKind::ClaudeCode,
                &SecretString::from(creds_json(access, "r", ms_from_now(3600))),
                PathBuf::from("/nonexistent/voidbox-injector-test/creds.json"),
            )
            .expect("build store"),
        )
    }

    #[tokio::test]
    async fn oauth_injects_bearer_and_drops_x_api_key() {
        let injector = OAuthBearerInjector::new(
            "api.anthropic.com",
            store_with_valid_token("oat-live-token"),
        );
        let mut headers = placeholder_headers();
        assert_eq!(
            injector.inject("api.anthropic.com", &mut headers).await,
            InjectOutcome::Injected
        );
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer oat-live-token"
        );
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("authorization").unwrap().is_sensitive());
    }

    #[tokio::test]
    async fn oauth_does_not_inject_for_other_hosts() {
        let injector = OAuthBearerInjector::new(
            "api.anthropic.com",
            store_with_valid_token("oat-live-token"),
        );
        let mut headers = HeaderMap::new();
        assert_eq!(
            injector.inject("evil.example.com", &mut headers).await,
            InjectOutcome::NotOwned
        );
        assert!(headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn oauth_fails_closed_and_drops_headers_when_store_cannot_mint() {
        // Expired token + an unreachable token endpoint: the refresh fails, so the
        // injector must report `Failed` and leave no credential header behind.
        let loopback = reqwest::Client::builder().build().unwrap();
        let store = Arc::new(
            OAuthTokenStore::from_json(
                OAuthProviderKind::ClaudeCode,
                &SecretString::from(creds_json("stale", "dead", ms_from_now(-60))),
                PathBuf::from("/nonexistent/voidbox-injector-test/creds.json"),
            )
            .unwrap()
            .with_token_endpoint("http://127.0.0.1:1/never")
            .with_http_client(loopback),
        );
        let injector = OAuthBearerInjector::new("api.anthropic.com", store);
        let mut headers = placeholder_headers();
        assert_eq!(
            injector.inject("api.anthropic.com", &mut headers).await,
            InjectOutcome::Failed
        );
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
    }
}
