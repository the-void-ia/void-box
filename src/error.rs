//! Error types for void-box

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result type alias using void-box Error
pub type Result<T> = std::result::Result<T, Error>;

/// Spec-compliant API error codes for the void-control orchestration contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApiErrorCode {
    InvalidSpec,
    InvalidPolicy,
    NotFound,
    AlreadyTerminal,
    ResourceLimitExceeded,
    InternalError,
}

/// Structured API error response: `{"code":"NOT_FOUND","message":"...","retryable":false}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::NotFound,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn invalid_spec(message: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::InvalidSpec,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn invalid_policy(message: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::InvalidPolicy,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn already_terminal(message: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::AlreadyTerminal,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn resource_limit_exceeded(message: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::ResourceLimitExceeded,
            message: message.into(),
            retryable: true,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::InternalError,
            message: message.into(),
            retryable: true,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"code":"INTERNAL_ERROR","message":"serialization failed","retryable":true}"#
                .to_string()
        })
    }
}

/// Errors that can occur in void-box operations
#[derive(Error, Debug)]
pub enum Error {
    /// KVM-related errors (Linux only)
    #[cfg(target_os = "linux")]
    #[error("KVM error: {0}")]
    Kvm(#[from] kvm_ioctls::Error),

    /// Backend-related errors (cross-platform)
    #[error("Backend error: {0}")]
    Backend(String),

    /// Memory-related errors
    #[error("Memory error: {0}")]
    Memory(String),

    /// Boot/kernel loading errors
    #[error("Boot error: {0}")]
    Boot(String),

    /// Device emulation errors
    #[error("Device error: {0}")]
    Device(String),

    /// Guest communication errors
    #[error("Guest communication error: {0}")]
    Guest(String),

    /// Network-related errors
    #[error("Network error: {0}")]
    Network(String),

    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    /// I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Timeout waiting for operation
    #[error("Timeout: {0}")]
    Timeout(String),

    /// VM is not running
    #[error("VM is not running")]
    VmNotRunning,

    /// VM is already running
    #[error("VM is already running")]
    VmAlreadyRunning,

    /// vCPU error
    #[error("vCPU error: {0}")]
    Vcpu(String),

    /// Serialization/deserialization errors
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// System call errors (Linux only — nix crate)
    #[cfg(target_os = "linux")]
    #[error("System error: {0}")]
    System(#[from] nix::Error),

    /// Workflow execution errors
    #[error("Workflow error: {0}")]
    Workflow(String),

    /// Step execution errors
    #[error("Step error: {0}")]
    Step(String),

    /// Sandbox errors
    #[error("Sandbox error: {0}")]
    Sandbox(String),

    /// Observability errors
    #[error("Observability error: {0}")]
    Observe(String),

    /// Protocol wire-format errors
    #[error("Protocol error: {0}")]
    Protocol(#[from] void_box_protocol::ProtocolError),
}
