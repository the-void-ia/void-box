//! Error types for void-box

use thiserror::Error;

/// Result type alias using void-box Error
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur in void-box operations
#[derive(Error, Debug)]
pub enum Error {
    /// KVM-related errors
    #[error("KVM error: {0}")]
    Kvm(#[from] kvm_ioctls::Error),

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

    /// System call errors
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
