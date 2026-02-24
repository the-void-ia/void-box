use serde::{Deserialize, Serialize};

use crate::{OciError, Result};

// ---------------------------------------------------------------------------
// OCI Image Manifest
// ---------------------------------------------------------------------------

/// An OCI image manifest (application/vnd.oci.image.manifest.v1+json or
/// application/vnd.docker.distribution.manifest.v2+json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciManifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,

    #[serde(rename = "mediaType", default)]
    pub media_type: String,

    pub config: Descriptor,

    pub layers: Vec<Descriptor>,
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

/// A content-addressable descriptor used in both manifests and image indexes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,

    pub digest: String,

    pub size: u64,

    #[serde(default)]
    pub platform: Option<Platform>,
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

/// Target platform for a manifest inside an image index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl Platform {
    /// Build a `Platform` matching the current host.
    pub fn host() -> Self {
        Self {
            architecture: host_arch().to_string(),
            os: "linux".to_string(),
            variant: None,
        }
    }
}

/// Map Rust `std::env::consts::ARCH` values to OCI / Docker platform strings.
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        "s390x" => "s390x",
        "powerpc64" => "ppc64le",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Image Index (fat manifest)
// ---------------------------------------------------------------------------

/// An OCI image index (application/vnd.oci.image.index.v1+json or
/// application/vnd.docker.distribution.manifest.list.v2+json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,

    pub manifests: Vec<Descriptor>,
}

impl ImageIndex {
    /// Select the descriptor whose platform matches the given target.
    pub fn select_platform(&self, target: &Platform) -> Result<&Descriptor> {
        self.manifests
            .iter()
            .find(|d| {
                if let Some(ref p) = d.platform {
                    p.architecture == target.architecture
                        && p.os == target.os
                        && (target.variant.is_none() || p.variant == target.variant)
                } else {
                    false
                }
            })
            .ok_or_else(|| {
                OciError::Manifest(format!(
                    "no manifest found for platform {}/{}",
                    target.os, target.architecture,
                ))
            })
    }
}

// ---------------------------------------------------------------------------
// Image Config
// ---------------------------------------------------------------------------

/// Top-level image configuration blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub config: Option<ContainerConfig>,
}

/// Container runtime configuration extracted from the image config blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Env", default)]
    pub env: Vec<String>,

    #[serde(rename = "Cmd", default)]
    pub cmd: Vec<String>,

    #[serde(rename = "WorkingDir", default)]
    pub working_dir: String,
}

// ---------------------------------------------------------------------------
// Media type constants
// ---------------------------------------------------------------------------

pub const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_TYPE_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_TYPE_DOCKER_MANIFEST_LIST: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MEDIA_TYPE_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Returns `true` when `media_type` is an image index / manifest list.
pub fn is_index_media_type(media_type: &str) -> bool {
    media_type == MEDIA_TYPE_OCI_INDEX || media_type == MEDIA_TYPE_DOCKER_MANIFEST_LIST
}

/// Returns `true` when `media_type` is a single image manifest.
pub fn is_manifest_media_type(media_type: &str) -> bool {
    media_type == MEDIA_TYPE_OCI_MANIFEST || media_type == MEDIA_TYPE_DOCKER_MANIFEST
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MANIFEST: &str = r#"{
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": "sha256:aaaa",
            "size": 1234
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": "sha256:bbbb",
                "size": 5678
            },
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": "sha256:cccc",
                "size": 91011
            }
        ]
    }"#;

    #[test]
    fn parse_manifest() {
        let m: OciManifest = serde_json::from_str(SAMPLE_MANIFEST).unwrap();
        assert_eq!(m.schema_version, 2);
        assert_eq!(m.config.digest, "sha256:aaaa");
        assert_eq!(m.layers.len(), 2);
        assert_eq!(m.layers[0].digest, "sha256:bbbb");
        assert_eq!(m.layers[1].size, 91011);
    }

    const SAMPLE_INDEX: &str = r#"{
        "schemaVersion": 2,
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": "sha256:amd64digest",
                "size": 100,
                "platform": { "architecture": "amd64", "os": "linux" }
            },
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": "sha256:arm64digest",
                "size": 200,
                "platform": { "architecture": "arm64", "os": "linux", "variant": "v8" }
            }
        ]
    }"#;

    #[test]
    fn select_platform_amd64() {
        let idx: ImageIndex = serde_json::from_str(SAMPLE_INDEX).unwrap();
        let target = Platform {
            architecture: "amd64".to_string(),
            os: "linux".to_string(),
            variant: None,
        };
        let desc = idx.select_platform(&target).unwrap();
        assert_eq!(desc.digest, "sha256:amd64digest");
    }

    #[test]
    fn select_platform_arm64() {
        let idx: ImageIndex = serde_json::from_str(SAMPLE_INDEX).unwrap();
        let target = Platform {
            architecture: "arm64".to_string(),
            os: "linux".to_string(),
            variant: None,
        };
        let desc = idx.select_platform(&target).unwrap();
        assert_eq!(desc.digest, "sha256:arm64digest");
    }

    #[test]
    fn select_platform_missing() {
        let idx: ImageIndex = serde_json::from_str(SAMPLE_INDEX).unwrap();
        let target = Platform {
            architecture: "s390x".to_string(),
            os: "linux".to_string(),
            variant: None,
        };
        assert!(idx.select_platform(&target).is_err());
    }

    #[test]
    fn media_type_helpers() {
        assert!(is_index_media_type(MEDIA_TYPE_OCI_INDEX));
        assert!(is_index_media_type(MEDIA_TYPE_DOCKER_MANIFEST_LIST));
        assert!(!is_index_media_type(MEDIA_TYPE_OCI_MANIFEST));

        assert!(is_manifest_media_type(MEDIA_TYPE_OCI_MANIFEST));
        assert!(is_manifest_media_type(MEDIA_TYPE_DOCKER_MANIFEST));
        assert!(!is_manifest_media_type(MEDIA_TYPE_OCI_INDEX));
    }
}
