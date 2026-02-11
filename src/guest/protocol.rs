//! Guest agent protocol definitions
//!
//! Defines the message format for communication between the host VMM
//! and the guest agent running inside the VM.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Message types for host-guest communication
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MessageType {
    /// Request to execute a command
    ExecRequest = 1,
    /// Response from command execution
    ExecResponse = 2,
    /// Ping request (health check)
    Ping = 3,
    /// Pong response (health check)
    Pong = 4,
    /// Shutdown request
    Shutdown = 5,
    /// File transfer request
    FileTransfer = 6,
    /// File transfer response
    FileTransferResponse = 7,
}

/// Wire format for messages
///
/// Format:
/// - 4 bytes: message length (little endian, not including header)
/// - 1 byte: message type
/// - N bytes: payload (JSON encoded)
#[derive(Debug, Clone)]
pub struct Message {
    /// Type of message
    pub msg_type: MessageType,
    /// Message payload (JSON encoded)
    pub payload: Vec<u8>,
}

impl Message {
    /// Header size: 4 bytes length + 1 byte type
    const HEADER_SIZE: usize = 5;

    /// Serialize message to bytes
    pub fn serialize(&self) -> Vec<u8> {
        let payload_len = self.payload.len() as u32;
        let mut buf = Vec::with_capacity(Self::HEADER_SIZE + self.payload.len());

        // Length (4 bytes, little endian)
        buf.extend_from_slice(&payload_len.to_le_bytes());
        // Type (1 byte)
        buf.push(self.msg_type as u8);
        // Payload
        buf.extend_from_slice(&self.payload);

        buf
    }

    /// Deserialize message from bytes
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        if data.len() < Self::HEADER_SIZE {
            return Err(Error::Guest("Message too short".into()));
        }

        let length = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let msg_type = Self::parse_message_type(data[4])?;

        if data.len() < Self::HEADER_SIZE + length {
            return Err(Error::Guest("Incomplete message".into()));
        }

        let payload = data[Self::HEADER_SIZE..Self::HEADER_SIZE + length].to_vec();

        Ok(Self { msg_type, payload })
    }

    /// Read a message from an async stream
    pub async fn read_from<R: tokio::io::AsyncReadExt + Unpin>(reader: &mut R) -> Result<Self> {
        // Read header
        let mut header = [0u8; Self::HEADER_SIZE];
        reader
            .read_exact(&mut header)
            .await
            .map_err(|e| Error::Guest(format!("Failed to read message header: {}", e)))?;

        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let msg_type = Self::parse_message_type(header[4])?;

        // Read payload
        let mut payload = vec![0u8; length];
        if length > 0 {
            reader
                .read_exact(&mut payload)
                .await
                .map_err(|e| Error::Guest(format!("Failed to read message payload: {}", e)))?;
        }

        Ok(Self { msg_type, payload })
    }

    /// Read a message from a synchronous stream
    pub fn read_from_sync<R: std::io::Read>(reader: &mut R) -> Result<Self> {
        // Read header
        let mut header = [0u8; Self::HEADER_SIZE];
        reader
            .read_exact(&mut header)
            .map_err(|e| Error::Guest(format!("Failed to read message header: {}", e)))?;

        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let msg_type = Self::parse_message_type(header[4])?;

        // Read payload
        let mut payload = vec![0u8; length];
        if length > 0 {
            reader
                .read_exact(&mut payload)
                .map_err(|e| Error::Guest(format!("Failed to read message payload: {}", e)))?;
        }

        Ok(Self { msg_type, payload })
    }

    fn parse_message_type(byte: u8) -> Result<MessageType> {
        match byte {
            1 => Ok(MessageType::ExecRequest),
            2 => Ok(MessageType::ExecResponse),
            3 => Ok(MessageType::Ping),
            4 => Ok(MessageType::Pong),
            5 => Ok(MessageType::Shutdown),
            6 => Ok(MessageType::FileTransfer),
            7 => Ok(MessageType::FileTransferResponse),
            _ => Err(Error::Guest(format!("Unknown message type: {}", byte))),
        }
    }
}

/// Request to execute a command in the guest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Program to execute
    pub program: String,
    /// Arguments to the program
    pub args: Vec<String>,
    /// Standard input data
    #[serde(default)]
    pub stdin: Vec<u8>,
    /// Environment variables
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory (optional)
    pub working_dir: Option<String>,
    /// Timeout in seconds (optional)
    pub timeout_secs: Option<u64>,
}

/// Response from command execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    /// Standard output
    pub stdout: Vec<u8>,
    /// Standard error
    pub stderr: Vec<u8>,
    /// Exit code
    pub exit_code: i32,
    /// Error message if execution failed
    pub error: Option<String>,
    /// Execution duration in milliseconds
    pub duration_ms: Option<u64>,
}

impl ExecResponse {
    /// Create a successful response
    pub fn success(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: i32, duration_ms: u64) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
            error: None,
            duration_ms: Some(duration_ms),
        }
    }

    /// Create an error response
    pub fn error(message: String) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: -1,
            error: Some(message),
            duration_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialize_deserialize() {
        let msg = Message {
            msg_type: MessageType::Ping,
            payload: b"hello".to_vec(),
        };

        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Ping);
        assert_eq!(decoded.payload, b"hello");
    }

    #[test]
    fn test_exec_request_serialize() {
        let req = ExecRequest {
            program: "echo".to_string(),
            args: vec!["hello".to_string()],
            stdin: Vec::new(),
            env: Vec::new(),
            working_dir: None,
            timeout_secs: Some(30),
        };

        let json = serde_json::to_string(&req).unwrap();
        let decoded: ExecRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.program, "echo");
        assert_eq!(decoded.args, vec!["hello"]);
    }

    #[test]
    fn test_exec_response() {
        let resp = ExecResponse::success(b"output".to_vec(), b"error".to_vec(), 0, 100);
        assert!(resp.error.is_none());
        assert_eq!(resp.exit_code, 0);

        let err_resp = ExecResponse::error("failed".to_string());
        assert!(err_resp.error.is_some());
        assert_eq!(err_resp.exit_code, -1);
    }
}
