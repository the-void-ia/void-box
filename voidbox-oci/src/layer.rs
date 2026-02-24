use std::path::PathBuf;

/// Metadata about a downloaded layer blob.
#[derive(Debug, Clone)]
pub struct LayerInfo {
    /// Content-addressable digest (e.g. "sha256:abcdef…").
    pub digest: String,
    /// Compressed size in bytes.
    pub size: u64,
    /// OCI media type (e.g. "application/vnd.oci.image.layer.v1.tar+gzip").
    pub media_type: String,
    /// Absolute path to the cached blob on disk.
    pub local_path: PathBuf,
}
