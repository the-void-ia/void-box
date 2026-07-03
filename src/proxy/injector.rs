//! Static API-key credential injector — the Phase-0 [`CredentialInjector`].
//!
//! Holds one host-held API key and rewrites the credential header for the exact
//! upstream host it owns. Exact-host matching is deliberate (R3): the injector
//! never attaches the secret to a request whose host differs from the one it was
//! configured for, so an agent-controlled `Host` header (which the proxy strips
//! anyway) or a misrouted connection cannot redirect the credential to another
//! destination. Path-scoped injection (crediting only specific paths on a host,
//! needed once non-LLM downstream services share this trait) is deferred to M2;
//! Phase 0 credits every path on its single name-constrained LLM upstream.
//!
//! Phase 1 adds an OAuth-backed [`CredentialInjector`] alongside this one — a
//! second implementation that mints a short-lived Bearer per call, selected per
//! provider and auth mode behind the same trait boundary. It does not replace
//! this injector: the API-key providers (Claude with an API key, the
//! Anthropic-compatible Custom provider, and codex API-key mode) keep using this
//! static path, which is the sanctioned path for programmatic use.

use http::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use tracing::warn;

use crate::proxy::{CredentialInjector, InjectOutcome};

/// Anthropic credential header.
const ANTHROPIC_API_KEY_HEADER: &str = "x-api-key";
/// Bearer credential header.
const AUTHORIZATION_HEADER: &str = "authorization";

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

impl CredentialInjector for StaticApiKeyInjector {
    fn inject(&self, host: &str, headers: &mut HeaderMap) -> InjectOutcome {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn anthropic_injects_x_api_key_and_drops_bearer() {
        let injector = StaticApiKeyInjector::new(
            "api.anthropic.com",
            ApiKeyScheme::AnthropicXApiKey,
            SecretString::from("sk-real-secret"),
        );
        let mut headers = placeholder_headers();
        assert_eq!(
            injector.inject("api.anthropic.com", &mut headers),
            InjectOutcome::Injected
        );

        assert_eq!(headers.get("x-api-key").unwrap(), "sk-real-secret");
        assert!(headers.get("authorization").is_none());
    }

    #[test]
    fn bearer_injects_authorization_and_drops_x_api_key() {
        let injector = StaticApiKeyInjector::new(
            "api.openai.com",
            ApiKeyScheme::Bearer,
            SecretString::from("sk-openai-secret"),
        );
        let mut headers = placeholder_headers();
        assert_eq!(
            injector.inject("api.openai.com", &mut headers),
            InjectOutcome::Injected
        );

        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer sk-openai-secret"
        );
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn does_not_inject_for_other_hosts() {
        let injector = StaticApiKeyInjector::new(
            "api.anthropic.com",
            ApiKeyScheme::AnthropicXApiKey,
            SecretString::from("sk-real-secret"),
        );
        let mut headers = HeaderMap::new();
        assert_eq!(
            injector.inject("evil.example.com", &mut headers),
            InjectOutcome::NotOwned
        );
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn injected_value_is_marked_sensitive() {
        let injector = StaticApiKeyInjector::new(
            "api.anthropic.com",
            ApiKeyScheme::AnthropicXApiKey,
            SecretString::from("sk-real-secret"),
        );
        let mut headers = HeaderMap::new();
        injector.inject("api.anthropic.com", &mut headers);
        assert!(headers.get("x-api-key").unwrap().is_sensitive());
    }

    #[test]
    fn malformed_key_for_owned_host_fails_and_drops_header() {
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
            injector.inject("api.anthropic.com", &mut headers),
            InjectOutcome::Failed
        );
        assert!(
            headers.get("x-api-key").is_none(),
            "a malformed key must not be forwarded as a header"
        );
    }
}
