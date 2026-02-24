use crate::error::{OciError, Result};
use crate::manifest::{
    self, ImageIndex, OciManifest, Platform, MEDIA_TYPE_DOCKER_MANIFEST,
    MEDIA_TYPE_DOCKER_MANIFEST_LIST, MEDIA_TYPE_OCI_INDEX, MEDIA_TYPE_OCI_MANIFEST,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, WWW_AUTHENTICATE};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// ImageRef
// ---------------------------------------------------------------------------

/// A parsed OCI image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Registry hostname (e.g. "registry-1.docker.io").
    pub registry: String,
    /// Repository path (e.g. "library/alpine").
    pub repository: String,
    /// Tag or digest reference (e.g. "latest" or "sha256:abc123").
    pub reference: String,
}

impl ImageRef {
    /// Parse a raw image reference string.
    ///
    /// Supported formats:
    /// - `alpine:latest`
    /// - `ubuntu`
    /// - `ghcr.io/foo/bar:v1`
    /// - `my.registry.io/org/repo@sha256:abc123`
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(OciError::Registry("empty image reference".to_string()));
        }

        // Split off the reference (tag or digest).
        let (name_part, reference) = if let Some(at_pos) = raw.find('@') {
            // Digest reference: everything after '@'.
            (&raw[..at_pos], raw[at_pos + 1..].to_string())
        } else if let Some(colon_pos) = raw.rfind(':') {
            // Possible tag — but we must make sure the colon is not inside
            // the registry hostname (e.g. "localhost:5000/repo").  A tag
            // colon always comes after the last '/'.
            let after_last_slash = raw.rfind('/').map(|p| p + 1).unwrap_or(0);
            if colon_pos > after_last_slash {
                (&raw[..colon_pos], raw[colon_pos + 1..].to_string())
            } else {
                (raw, "latest".to_string())
            }
        } else {
            (raw, "latest".to_string())
        };

        // Determine registry vs repository.  A component is treated as a
        // registry hostname when it contains a dot or a colon (port).
        let (registry, repository) = if let Some(slash_pos) = name_part.find('/') {
            let first = &name_part[..slash_pos];
            if first.contains('.') || first.contains(':') {
                (first.to_string(), name_part[slash_pos + 1..].to_string())
            } else {
                // No dot/colon → treat as Docker Hub path component.
                ("registry-1.docker.io".to_string(), name_part.to_string())
            }
        } else {
            // Single-component name like "alpine" → Docker Hub official image.
            (
                "registry-1.docker.io".to_string(),
                format!("library/{}", name_part),
            )
        };

        // Docker Hub official images without "library/" prefix.
        let repository = if registry == "registry-1.docker.io" && !repository.contains('/') {
            format!("library/{}", repository)
        } else {
            repository
        };

        Ok(Self {
            registry,
            repository,
            reference,
        })
    }
}

// ---------------------------------------------------------------------------
// RegistryClient
// ---------------------------------------------------------------------------

/// Low-level OCI Distribution HTTP client.
pub struct RegistryClient {
    client: reqwest::Client,
}

/// Return the base URL scheme for a registry host.
/// Localhost and loopback registries default to HTTP; everything else to HTTPS.
fn registry_scheme(registry: &str) -> &'static str {
    let host = registry.split(':').next().unwrap_or(registry);
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        "http"
    } else {
        "https"
    }
}

impl RegistryClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("voidbox-oci/0.1")
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }

    // -- public API ---------------------------------------------------------

    /// Fetch the manifest (or image index) for `image_ref`.
    ///
    /// When the registry returns an image index the caller receives
    /// `ManifestResponse::Index`; for a single manifest it receives
    /// `ManifestResponse::Manifest`.
    pub async fn fetch_manifest(&self, image_ref: &ImageRef) -> Result<ManifestResponse> {
        let scheme = registry_scheme(&image_ref.registry);
        let url = format!(
            "{}://{}/v2/{}/manifests/{}",
            scheme, image_ref.registry, image_ref.repository, image_ref.reference,
        );

        let accept = [
            MEDIA_TYPE_OCI_INDEX,
            MEDIA_TYPE_DOCKER_MANIFEST_LIST,
            MEDIA_TYPE_OCI_MANIFEST,
            MEDIA_TYPE_DOCKER_MANIFEST,
        ]
        .join(", ");

        let body = self
            .authenticated_get(&url, image_ref, Some(&accept))
            .await?;

        // Peek at the response to decide which type to deserialize.
        let raw: serde_json::Value = serde_json::from_slice(&body)?;
        let media_type = raw.get("mediaType").and_then(|v| v.as_str()).unwrap_or("");

        if manifest::is_index_media_type(media_type) || raw.get("manifests").is_some() {
            let idx: ImageIndex = serde_json::from_value(raw)?;
            Ok(ManifestResponse::Index(idx))
        } else {
            let m: OciManifest = serde_json::from_value(raw)?;
            Ok(ManifestResponse::Manifest(m))
        }
    }

    /// Fetch a single manifest by its digest (used after resolving an index).
    pub async fn fetch_manifest_by_digest(
        &self,
        image_ref: &ImageRef,
        digest: &str,
    ) -> Result<OciManifest> {
        let scheme = registry_scheme(&image_ref.registry);
        let url = format!(
            "{}://{}/v2/{}/manifests/{}",
            scheme, image_ref.registry, image_ref.repository, digest,
        );

        let accept = [MEDIA_TYPE_OCI_MANIFEST, MEDIA_TYPE_DOCKER_MANIFEST].join(", ");

        let body = self
            .authenticated_get(&url, image_ref, Some(&accept))
            .await?;

        let m: OciManifest = serde_json::from_slice(&body)?;
        Ok(m)
    }

    /// Download a blob by digest.  Returns the raw bytes.
    pub async fn fetch_blob(&self, image_ref: &ImageRef, digest: &str) -> Result<Vec<u8>> {
        let scheme = registry_scheme(&image_ref.registry);
        let url = format!(
            "{}://{}/v2/{}/blobs/{}",
            scheme, image_ref.registry, image_ref.repository, digest,
        );

        self.authenticated_get(&url, image_ref, None).await
    }

    /// Download a blob and store it in `cache_dir/blobs/sha256/<hex>`.
    /// Verifies SHA-256 digest.  Returns the path to the cached file.
    pub async fn fetch_blob_to_cache(
        &self,
        image_ref: &ImageRef,
        digest: &str,
        cache: &crate::cache::BlobCache,
    ) -> Result<PathBuf> {
        if cache.has_blob(digest) {
            debug!(digest, "blob already cached");
            return Ok(cache.blob_path(digest));
        }

        info!(digest, "downloading blob");
        let data = self.fetch_blob(image_ref, digest).await?;

        // Verify digest.
        let hex = hex_digest(&data);
        let expected_hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        if hex != expected_hex {
            return Err(OciError::DigestMismatch {
                expected: expected_hex.to_string(),
                actual: hex,
            });
        }

        cache.store_blob(digest, &data).await
    }

    // -- internals ----------------------------------------------------------

    /// Perform a GET with anonymous-then-bearer-token auth flow.
    async fn authenticated_get(
        &self,
        url: &str,
        image_ref: &ImageRef,
        accept: Option<&str>,
    ) -> Result<Vec<u8>> {
        let mut req = self.client.get(url);
        if let Some(a) = accept {
            req = req.header(ACCEPT, a);
        }

        let resp = req.send().await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            // Extract www-authenticate and fetch a token.
            let www_auth = resp
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let token = self.fetch_bearer_token(&www_auth, image_ref).await?;

            // Retry with token.
            let mut req2 = self
                .client
                .get(url)
                .header(AUTHORIZATION, format!("Bearer {}", token));
            if let Some(a) = accept {
                req2 = req2.header(ACCEPT, a);
            }
            let resp2 = req2.send().await?;

            if !resp2.status().is_success() {
                let status = resp2.status();
                let body = resp2.text().await.unwrap_or_default();
                return Err(OciError::Registry(format!(
                    "GET {} returned {}: {}",
                    url, status, body
                )));
            }

            Ok(resp2.bytes().await?.to_vec())
        } else if resp.status() == StatusCode::NOT_FOUND {
            Err(OciError::NotFound(url.to_string()))
        } else if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(OciError::Registry(format!(
                "GET {} returned {}: {}",
                url, status, body
            )))
        } else {
            Ok(resp.bytes().await?.to_vec())
        }
    }

    /// Parse a `www-authenticate: Bearer realm="…",service="…",scope="…"`
    /// header and fetch an anonymous token.
    async fn fetch_bearer_token(&self, www_auth: &str, image_ref: &ImageRef) -> Result<String> {
        let realm = extract_param(www_auth, "realm").unwrap_or_default();
        let service = extract_param(www_auth, "service").unwrap_or_default();
        let scope = extract_param(www_auth, "scope")
            .unwrap_or_else(|| format!("repository:{}:pull", image_ref.repository));

        if realm.is_empty() {
            return Err(OciError::Registry(
                "www-authenticate header missing realm".to_string(),
            ));
        }

        let token_url = format!("{}?service={}&scope={}", realm, service, scope);
        debug!(%token_url, "fetching bearer token");

        let resp = self.client.get(&token_url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OciError::Registry(format!(
                "token endpoint returned {}: {}",
                status, body
            )));
        }

        let body: serde_json::Value = resp.json().await?;
        let token = body
            .get("token")
            .or_else(|| body.get("access_token"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| OciError::Registry("token response missing token field".to_string()))?;

        Ok(token.to_string())
    }

    /// Resolve an image reference to a concrete [`OciManifest`] by first
    /// fetching the manifest (which may be an index) and selecting the
    /// platform-appropriate entry if needed.
    pub async fn resolve_manifest(
        &self,
        image_ref: &ImageRef,
        platform: &Platform,
    ) -> Result<OciManifest> {
        match self.fetch_manifest(image_ref).await? {
            ManifestResponse::Manifest(m) => Ok(m),
            ManifestResponse::Index(idx) => {
                let desc = idx.select_platform(platform)?;
                info!(
                    digest = %desc.digest,
                    "resolved platform {}/{}",
                    platform.os,
                    platform.architecture,
                );
                self.fetch_manifest_by_digest(image_ref, &desc.digest).await
            }
        }
    }
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ManifestResponse
// ---------------------------------------------------------------------------

/// The result of fetching a manifest endpoint — either a single manifest or
/// an image index that must be further resolved.
pub enum ManifestResponse {
    Manifest(OciManifest),
    Index(ImageIndex),
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the SHA-256 hex digest of `data`.
fn hex_digest(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hex_encode(&hash)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Extract a parameter value from a `www-authenticate` header.
/// E.g. `extract_param(header, "realm")` returns the value of `realm="…"`.
fn extract_param(header: &str, param: &str) -> Option<String> {
    let search = format!("{}=\"", param);
    if let Some(start) = header.find(&search) {
        let value_start = start + search.len();
        if let Some(end) = header[value_start..].find('"') {
            return Some(header[value_start..value_start + end].to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_name() {
        let r = ImageRef::parse("ubuntu").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_name_with_tag() {
        let r = ImageRef::parse("alpine:latest").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_ghcr() {
        let r = ImageRef::parse("ghcr.io/foo/bar:v1").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "foo/bar");
        assert_eq!(r.reference, "v1");
    }

    #[test]
    fn parse_digest_reference() {
        let r = ImageRef::parse("my.registry.io/org/repo@sha256:abc123").unwrap();
        assert_eq!(r.registry, "my.registry.io");
        assert_eq!(r.repository, "org/repo");
        assert_eq!(r.reference, "sha256:abc123");
    }

    #[test]
    fn parse_docker_hub_user_repo() {
        let r = ImageRef::parse("myuser/myrepo:v2").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "myuser/myrepo");
        assert_eq!(r.reference, "v2");
    }

    #[test]
    fn parse_registry_with_port() {
        let r = ImageRef::parse("localhost:5000/myrepo:tag").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "myrepo");
        assert_eq!(r.reference, "tag");
    }

    #[test]
    fn parse_empty_returns_error() {
        assert!(ImageRef::parse("").is_err());
    }

    #[test]
    fn registry_scheme_localhost_is_http() {
        assert_eq!(registry_scheme("localhost:5555"), "http");
        assert_eq!(registry_scheme("localhost:5000"), "http");
        assert_eq!(registry_scheme("localhost"), "http");
        assert_eq!(registry_scheme("127.0.0.1:5000"), "http");
    }

    #[test]
    fn registry_scheme_remote_is_https() {
        assert_eq!(registry_scheme("ghcr.io"), "https");
        assert_eq!(registry_scheme("registry-1.docker.io"), "https");
        assert_eq!(registry_scheme("my.registry.io:443"), "https");
    }

    #[test]
    fn extract_param_works() {
        let header = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        assert_eq!(
            extract_param(header, "realm"),
            Some("https://auth.docker.io/token".to_string())
        );
        assert_eq!(
            extract_param(header, "service"),
            Some("registry.docker.io".to_string())
        );
        assert_eq!(
            extract_param(header, "scope"),
            Some("repository:library/alpine:pull".to_string())
        );
        assert_eq!(extract_param(header, "missing"), None);
    }
}
