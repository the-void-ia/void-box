pub mod cache;
pub mod error;
pub mod layer;
pub mod manifest;
pub mod registry;
pub mod unpack;

pub use error::{OciError, Result};

use std::path::{Path, PathBuf};
use tracing::info;

/// OCI image client -- pulls, caches, and unpacks container images.
pub struct OciClient {
    cache_dir: PathBuf,
    registry: registry::RegistryClient,
    platform: manifest::Platform,
}

/// A fully-pulled OCI image: manifest, layer metadata, and container config.
pub struct PulledImage {
    pub manifest: manifest::OciManifest,
    pub layers: Vec<layer::LayerInfo>,
    pub config: manifest::ImageConfig,
}

impl OciClient {
    /// Create a new `OciClient` that caches blobs and rootfs trees under
    /// `cache_dir`.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            registry: registry::RegistryClient::new(),
            platform: manifest::Platform::host(),
        }
    }

    /// Pull an image manifest and all layers, returning [`PulledImage`]
    /// metadata.  Layers are downloaded into the content-addressed blob cache
    /// and are not yet extracted.
    pub async fn pull(&self, image_ref: &str) -> Result<PulledImage> {
        let parsed = registry::ImageRef::parse(image_ref)?;
        let blob_cache = cache::BlobCache::new(self.cache_dir.clone());

        info!(
            registry = %parsed.registry,
            repository = %parsed.repository,
            reference = %parsed.reference,
            "pulling image",
        );

        // 1. Resolve manifest (handles image index → platform selection).
        let manifest = self
            .registry
            .resolve_manifest(&parsed, &self.platform)
            .await?;

        // 2. Download the image config blob.
        let config_path = self
            .registry
            .fetch_blob_to_cache(&parsed, &manifest.config.digest, &blob_cache)
            .await?;
        let config_bytes = tokio::fs::read(&config_path).await?;
        let config: manifest::ImageConfig = serde_json::from_slice(&config_bytes)?;

        // 3. Download each layer blob.
        let mut layers = Vec::with_capacity(manifest.layers.len());
        for desc in &manifest.layers {
            let local_path = self
                .registry
                .fetch_blob_to_cache(&parsed, &desc.digest, &blob_cache)
                .await?;
            layers.push(layer::LayerInfo {
                digest: desc.digest.clone(),
                size: desc.size,
                media_type: desc.media_type.clone(),
                local_path,
            });
        }

        Ok(PulledImage {
            manifest,
            layers,
            config,
        })
    }

    /// Unpack a previously pulled image's layers into `dest`, producing a
    /// merged root filesystem.  Returns the rootfs path.
    pub async fn unpack(&self, image: &PulledImage, dest: &Path) -> Result<PathBuf> {
        // Unpacking is CPU-bound; run on the blocking pool.
        let layers = image.layers.clone();
        let dest = dest.to_path_buf();
        let rootfs = tokio::task::spawn_blocking(move || unpack::unpack_layers(&layers, &dest))
            .await
            .map_err(|e| OciError::Layer(format!("unpack task panicked: {}", e)))??;
        Ok(rootfs)
    }

    /// Convenience method: pull + unpack + cache.  Returns the path to the
    /// extracted rootfs directory.
    ///
    /// If the rootfs has already been cached the pull and unpack are skipped.
    pub async fn resolve_rootfs(&self, image_ref: &str) -> Result<PathBuf> {
        let blob_cache = cache::BlobCache::new(self.cache_dir.clone());

        // Use a simple hash of the image ref as the cache key for the rootfs.
        // (A production implementation would use the manifest digest.)
        let cache_key = format!("sha256:{}", simple_hash(image_ref));

        if blob_cache.has_rootfs(&cache_key) {
            let rootfs = blob_cache.rootfs_path(&cache_key);
            info!(path = %rootfs.display(), "using cached rootfs");
            return Ok(rootfs);
        }

        // Remove any leftover partial extraction from a previous failed run.
        let rootfs_dir = blob_cache.rootfs_path(&cache_key);
        if rootfs_dir.exists() {
            info!(path = %rootfs_dir.display(), "removing incomplete rootfs");
            let _ = tokio::fs::remove_dir_all(&rootfs_dir).await;
        }

        let image = self.pull(image_ref).await?;
        let rootfs = self.unpack(&image, &rootfs_dir).await?;

        // Mark as successfully completed so future runs use the cache.
        blob_cache.mark_rootfs_done(&cache_key).await?;

        info!(path = %rootfs.display(), "rootfs ready");
        Ok(rootfs)
    }
}

/// Produce a deterministic hex string from `input` (not cryptographic, just
/// for cache key purposes).
fn simple_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(input.as_bytes());
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}
