/// Errors produced by the OCI client.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    #[error("registry error: {0}")]
    Registry(String),

    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("layer error: {0}")]
    Layer(String),

    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },

    #[error("unsupported media type: {0}")]
    UnsupportedMediaType(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not found: {0}")]
    NotFound(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, OciError>;
