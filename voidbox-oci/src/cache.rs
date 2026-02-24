use crate::error::Result;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

/// Content-addressed blob cache stored under `<cache_dir>/blobs/sha256/<hex>`.
pub struct BlobCache {
    cache_dir: PathBuf,
}

impl BlobCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Directory that holds all blobs: `<cache_dir>/blobs/sha256/`.
    fn blobs_dir(&self) -> PathBuf {
        self.cache_dir.join("blobs").join("sha256")
    }

    /// Extract the hex portion from a digest string like "sha256:abcdef…".
    fn hex_from_digest(digest: &str) -> &str {
        digest.strip_prefix("sha256:").unwrap_or(digest)
    }

    /// Check whether a blob for `digest` already exists on disk.
    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).exists()
    }

    /// Return the expected path for a blob with the given digest.
    pub fn blob_path(&self, digest: &str) -> PathBuf {
        self.blobs_dir().join(Self::hex_from_digest(digest))
    }

    /// Persist `data` into the cache under `digest`.  Returns the file path.
    pub async fn store_blob(&self, digest: &str, data: &[u8]) -> Result<PathBuf> {
        let dir = self.blobs_dir();
        fs::create_dir_all(&dir).await?;

        let path = dir.join(Self::hex_from_digest(digest));
        fs::write(&path, data).await?;
        debug!(path = %path.display(), "stored blob");
        Ok(path)
    }

    /// Path where an unpacked rootfs for `image_digest` will reside.
    pub fn rootfs_path(&self, image_digest: &str) -> PathBuf {
        self.cache_dir
            .join("rootfs")
            .join(Self::hex_from_digest(image_digest))
    }

    /// Check whether a rootfs has already been fully unpacked for `image_digest`.
    pub fn has_rootfs(&self, image_digest: &str) -> bool {
        self.rootfs_done_marker(image_digest).exists()
    }

    /// Mark a rootfs extraction as complete.
    pub async fn mark_rootfs_done(&self, image_digest: &str) -> Result<()> {
        let marker = self.rootfs_done_marker(image_digest);
        fs::write(&marker, b"done").await?;
        Ok(())
    }

    /// Path to the completion marker for a rootfs.
    fn rootfs_done_marker(&self, image_digest: &str) -> PathBuf {
        self.rootfs_path(image_digest).with_extension("done")
    }

    /// Return a reference to the underlying cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_path_strips_prefix() {
        let cache = BlobCache::new(PathBuf::from("/tmp/oci-cache"));
        let p = cache.blob_path("sha256:deadbeef");
        assert_eq!(p, PathBuf::from("/tmp/oci-cache/blobs/sha256/deadbeef"));
    }

    #[test]
    fn blob_path_no_prefix() {
        let cache = BlobCache::new(PathBuf::from("/tmp/oci-cache"));
        let p = cache.blob_path("deadbeef");
        assert_eq!(p, PathBuf::from("/tmp/oci-cache/blobs/sha256/deadbeef"));
    }

    #[test]
    fn rootfs_path_structure() {
        let cache = BlobCache::new(PathBuf::from("/tmp/oci-cache"));
        let p = cache.rootfs_path("sha256:abcd1234");
        assert_eq!(p, PathBuf::from("/tmp/oci-cache/rootfs/abcd1234"));
    }

    #[test]
    fn has_blob_returns_false_for_missing() {
        let cache = BlobCache::new(PathBuf::from("/tmp/nonexistent-oci-cache-test"));
        assert!(!cache.has_blob("sha256:000000"));
    }

    #[tokio::test]
    async fn store_and_retrieve_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(tmp.path().to_path_buf());

        let digest = "sha256:cafebabe";
        let data = b"hello world";

        assert!(!cache.has_blob(digest));

        let path = cache.store_blob(digest, data).await.unwrap();
        assert!(path.exists());
        assert!(cache.has_blob(digest));

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, data);
    }
}
