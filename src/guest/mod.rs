//! Guest communication module
//!
//! This module contains the protocol and types for host-guest communication
//! via vsock or other transport mechanisms.

pub mod protocol;

pub use protocol::{ExecRequest, ExecResponse, Message, MessageType};
