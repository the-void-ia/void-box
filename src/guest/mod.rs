//! Guest communication module
//!
//! This module contains the protocol and types for host-guest communication
//! via vsock or other transport mechanisms.

pub mod protocol;

pub use protocol::{
    read_message_async, ExecRequest, ExecResponse, Message, MessageType, ProcessMetrics,
    SystemMetrics, TelemetryBatch,
};
