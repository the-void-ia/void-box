//! Shared wire-format types for void-box host ↔ guest communication.
//!
//! This crate is the single source of truth for the message protocol used
//! between the host VMM (`void-box`) and the guest agent (`guest-agent`).
//! Both crates depend on this to avoid struct duplication.
//!
//! ## Wire Format
//!
//! Every message is framed as:
//!
//! ```text
//! ┌──────────────┬───────────┬──────────────────┐
//! │ length (4 B) │ type (1B) │ payload (N bytes) │
//! └──────────────┴───────────┴──────────────────┘
//! ```
//!
//! - **length**: `u32` little-endian, size of the payload only (not including the 5-byte header).
//! - **type**: one byte mapping to [`MessageType`].
//! - **payload**: JSON-encoded body (may be empty).

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors that can occur during protocol message parsing.
#[derive(Debug)]
pub enum ProtocolError {
    /// Message buffer too short or incomplete.
    InvalidMessage(String),
    /// The type byte does not map to a known [`MessageType`].
    UnknownMessageType(u8),
    /// An I/O error occurred while reading or writing.
    Io(std::io::Error),
    /// JSON (de)serialization failed.
    Json(serde_json::Error),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::InvalidMessage(msg) => write!(f, "Invalid message: {}", msg),
            ProtocolError::UnknownMessageType(b) => write!(f, "Unknown message type: {}", b),
            ProtocolError::Io(e) => write!(f, "IO error: {}", e),
            ProtocolError::Json(e) => write!(f, "JSON error: {}", e),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<std::io::Error> for ProtocolError {
    fn from(e: std::io::Error) -> Self {
        ProtocolError::Io(e)
    }
}

impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        ProtocolError::Json(e)
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Header size in bytes: 4 (length) + 1 (type).
pub const HEADER_SIZE: usize = 5;

// ---------------------------------------------------------------------------
// MessageType
// ---------------------------------------------------------------------------

/// Message types for host-guest communication.
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
    /// Telemetry data from guest
    TelemetryData = 8,
    /// Telemetry acknowledgement
    TelemetryAck = 9,
    /// Subscribe to telemetry stream
    SubscribeTelemetry = 10,
    /// Write a file to the guest filesystem (native, no shell required)
    WriteFile = 11,
    /// Response to WriteFile
    WriteFileResponse = 12,
    /// Create directories (mkdir -p) in the guest filesystem
    MkdirP = 13,
    /// Response to MkdirP
    MkdirPResponse = 14,
    /// Incremental stdout/stderr chunk during execution
    ExecOutputChunk = 15,
    /// Ack from host (optional flow control)
    ExecOutputAck = 16,
}

impl TryFrom<u8> for MessageType {
    type Error = ProtocolError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            1 => Ok(MessageType::ExecRequest),
            2 => Ok(MessageType::ExecResponse),
            3 => Ok(MessageType::Ping),
            4 => Ok(MessageType::Pong),
            5 => Ok(MessageType::Shutdown),
            6 => Ok(MessageType::FileTransfer),
            7 => Ok(MessageType::FileTransferResponse),
            8 => Ok(MessageType::TelemetryData),
            9 => Ok(MessageType::TelemetryAck),
            10 => Ok(MessageType::SubscribeTelemetry),
            11 => Ok(MessageType::WriteFile),
            12 => Ok(MessageType::WriteFileResponse),
            13 => Ok(MessageType::MkdirP),
            14 => Ok(MessageType::MkdirPResponse),
            15 => Ok(MessageType::ExecOutputChunk),
            16 => Ok(MessageType::ExecOutputAck),
            _ => Err(ProtocolError::UnknownMessageType(byte)),
        }
    }
}

// ---------------------------------------------------------------------------
// Message (wire frame)
// ---------------------------------------------------------------------------

/// A framed protocol message consisting of a type tag and a payload.
///
/// Use [`Message::serialize`] / [`Message::deserialize`] for in-memory
/// conversion and [`Message::read_from_sync`] for streaming from a reader.
#[derive(Debug, Clone)]
pub struct Message {
    /// Type of message.
    pub msg_type: MessageType,
    /// Message payload (typically JSON-encoded).
    pub payload: Vec<u8>,
}

impl Message {
    /// Serialize this message into a byte buffer (header + payload).
    pub fn serialize(&self) -> Vec<u8> {
        let payload_len = self.payload.len() as u32;
        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.push(self.msg_type as u8);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Deserialize a message from a contiguous byte slice.
    pub fn deserialize(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < HEADER_SIZE {
            return Err(ProtocolError::InvalidMessage("Message too short".into()));
        }

        let length = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let msg_type = MessageType::try_from(data[4])?;

        if data.len() < HEADER_SIZE + length {
            return Err(ProtocolError::InvalidMessage("Incomplete message".into()));
        }

        let payload = data[HEADER_SIZE..HEADER_SIZE + length].to_vec();
        Ok(Self { msg_type, payload })
    }

    /// Read a complete message from a synchronous [`std::io::Read`] stream.
    pub fn read_from_sync<R: std::io::Read>(reader: &mut R) -> Result<Self, ProtocolError> {
        let mut header = [0u8; HEADER_SIZE];
        reader.read_exact(&mut header)?;

        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let msg_type = MessageType::try_from(header[4])?;

        let mut payload = vec![0u8; length];
        if length > 0 {
            reader.read_exact(&mut payload)?;
        }

        Ok(Self { msg_type, payload })
    }
}

// ---------------------------------------------------------------------------
// Data types: Exec
// ---------------------------------------------------------------------------

/// Request to execute a command in the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Program to execute.
    pub program: String,
    /// Arguments to the program.
    pub args: Vec<String>,
    /// Standard input data.
    #[serde(default)]
    pub stdin: Vec<u8>,
    /// Environment variables.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory (optional).
    pub working_dir: Option<String>,
    /// Timeout in seconds (optional).
    pub timeout_secs: Option<u64>,
}

/// Response from command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    /// Standard output.
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
    /// Exit code.
    pub exit_code: i32,
    /// Error message if execution failed.
    pub error: Option<String>,
    /// Execution duration in milliseconds.
    pub duration_ms: Option<u64>,
}

impl ExecResponse {
    /// Create a successful response.
    pub fn success(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: i32, duration_ms: u64) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
            error: None,
            duration_ms: Some(duration_ms),
        }
    }

    /// Create an error response.
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

/// Incremental stdout/stderr chunk sent during command execution.
///
/// The guest-agent sends these as output is produced. The final
/// [`ExecResponse`] still contains the complete output for backward
/// compatibility — hosts that don't understand this message type can
/// safely ignore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutputChunk {
    /// Which stream: `"stdout"` or `"stderr"`.
    pub stream: String,
    /// The data chunk.
    pub data: Vec<u8>,
    /// Sequence number for ordering.
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// Data types: File operations (native, no shell required)
// ---------------------------------------------------------------------------

/// Request to write a file in the guest filesystem.
///
/// The guest-agent handles this directly (no shell/base64 needed).
/// Parent directories are created automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileRequest {
    /// Absolute path in the guest filesystem.
    pub path: String,
    /// File content (binary-safe via serde).
    pub content: Vec<u8>,
    /// If true, create parent directories automatically.
    #[serde(default = "default_true")]
    pub create_parents: bool,
}

fn default_true() -> bool {
    true
}

/// Response to a WriteFile request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileResponse {
    /// Whether the write succeeded.
    pub success: bool,
    /// Error message if the write failed.
    pub error: Option<String>,
}

/// Request to create directories in the guest filesystem (mkdir -p).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MkdirPRequest {
    /// Absolute path to create.
    pub path: String,
}

/// Response to a MkdirP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MkdirPResponse {
    /// Whether the mkdir succeeded.
    pub success: bool,
    /// Error message if the mkdir failed.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Data types: Telemetry
// ---------------------------------------------------------------------------

/// A batch of telemetry data from the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryBatch {
    /// Monotonic sequence number for ordering.
    pub seq: u64,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// System-wide metrics.
    pub system: Option<SystemMetrics>,
    /// Per-process metrics.
    pub processes: Vec<ProcessMetrics>,
    /// W3C traceparent for correlation.
    pub trace_context: Option<String>,
}

/// System-wide metrics collected from procfs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// CPU usage percentage (from /proc/stat deltas).
    pub cpu_percent: f64,
    /// Memory used in bytes (MemTotal - MemAvailable).
    pub memory_used_bytes: u64,
    /// Total memory in bytes.
    pub memory_total_bytes: u64,
    /// Network bytes received (from /proc/net/dev).
    pub net_rx_bytes: u64,
    /// Network bytes transmitted.
    pub net_tx_bytes: u64,
    /// Number of running processes (from /proc/stat).
    pub procs_running: u32,
    /// Number of open file descriptors (from /proc/sys/fs/file-nr).
    pub open_fds: u32,
}

/// Per-process metrics collected from procfs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMetrics {
    /// Process ID.
    pub pid: u32,
    /// Command name (from /proc/PID/comm).
    pub comm: String,
    /// Resident set size in bytes (from /proc/PID/statm).
    pub rss_bytes: u64,
    /// CPU time in jiffies (utime + stime from /proc/PID/stat).
    pub cpu_jiffies: u64,
    /// Process state (R, S, D, Z, etc.).
    pub state: char,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip() {
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
    fn message_empty_payload() {
        let msg = Message {
            msg_type: MessageType::SubscribeTelemetry,
            payload: vec![],
        };
        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::SubscribeTelemetry);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn message_type_try_from_valid() {
        for &(byte, expected) in &[
            (1u8, MessageType::ExecRequest),
            (2, MessageType::ExecResponse),
            (3, MessageType::Ping),
            (4, MessageType::Pong),
            (5, MessageType::Shutdown),
            (6, MessageType::FileTransfer),
            (7, MessageType::FileTransferResponse),
            (8, MessageType::TelemetryData),
            (9, MessageType::TelemetryAck),
            (10, MessageType::SubscribeTelemetry),
        ] {
            assert_eq!(MessageType::try_from(byte).unwrap(), expected);
        }
    }

    #[test]
    fn message_type_try_from_invalid() {
        assert!(MessageType::try_from(0).is_err());
        assert!(MessageType::try_from(17).is_err());
        assert!(MessageType::try_from(255).is_err());
    }

    #[test]
    fn message_deserialize_too_short() {
        assert!(Message::deserialize(&[0, 0]).is_err());
    }

    #[test]
    fn message_deserialize_incomplete() {
        // Header says 10 bytes payload but only 2 present
        let data = [10, 0, 0, 0, 1, 0xAA, 0xBB];
        assert!(Message::deserialize(&data).is_err());
    }

    #[test]
    fn exec_request_json_round_trip() {
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
        assert_eq!(decoded.timeout_secs, Some(30));
    }

    #[test]
    fn exec_response_helpers() {
        let ok = ExecResponse::success(b"out".to_vec(), b"err".to_vec(), 0, 100);
        assert!(ok.error.is_none());
        assert_eq!(ok.exit_code, 0);
        assert_eq!(ok.duration_ms, Some(100));

        let err = ExecResponse::error("boom".to_string());
        assert!(err.error.is_some());
        assert_eq!(err.exit_code, -1);
    }

    #[test]
    fn telemetry_batch_json_round_trip() {
        let batch = TelemetryBatch {
            seq: 1,
            timestamp_ms: 1700000000000,
            system: Some(SystemMetrics {
                cpu_percent: 42.5,
                memory_used_bytes: 512 * 1024 * 1024,
                memory_total_bytes: 1024 * 1024 * 1024,
                net_rx_bytes: 1000,
                net_tx_bytes: 2000,
                procs_running: 3,
                open_fds: 128,
            }),
            processes: vec![ProcessMetrics {
                pid: 1,
                comm: "init".to_string(),
                rss_bytes: 4096,
                cpu_jiffies: 100,
                state: 'S',
            }],
            trace_context: None,
        };

        let json = serde_json::to_vec(&batch).unwrap();
        let decoded: TelemetryBatch = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.seq, 1);
        assert_eq!(decoded.system.as_ref().unwrap().cpu_percent, 42.5);
        assert_eq!(decoded.processes.len(), 1);
        assert_eq!(decoded.processes[0].comm, "init");
    }

    #[test]
    fn read_from_sync_round_trip() {
        let msg = Message {
            msg_type: MessageType::ExecRequest,
            payload: b"{\"program\":\"ls\"}".to_vec(),
        };
        let bytes = msg.serialize();
        let mut cursor = std::io::Cursor::new(bytes);
        let decoded = Message::read_from_sync(&mut cursor).unwrap();
        assert_eq!(decoded.msg_type, MessageType::ExecRequest);
        assert_eq!(decoded.payload, b"{\"program\":\"ls\"}");
    }

    #[test]
    fn exec_output_chunk_json_round_trip() {
        let chunk = ExecOutputChunk {
            stream: "stdout".to_string(),
            data: b"hello world\n".to_vec(),
            seq: 42,
        };
        let json = serde_json::to_vec(&chunk).unwrap();
        let decoded: ExecOutputChunk = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.stream, "stdout");
        assert_eq!(decoded.data, b"hello world\n");
        assert_eq!(decoded.seq, 42);
    }

    #[test]
    fn exec_output_chunk_message_type() {
        assert_eq!(
            MessageType::try_from(15).unwrap(),
            MessageType::ExecOutputChunk
        );
        assert_eq!(
            MessageType::try_from(16).unwrap(),
            MessageType::ExecOutputAck
        );
    }

    #[test]
    fn telemetry_message_types() {
        // TelemetryData
        let msg = Message {
            msg_type: MessageType::TelemetryData,
            payload: b"{}".to_vec(),
        };
        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::TelemetryData);

        // SubscribeTelemetry
        let msg = Message {
            msg_type: MessageType::SubscribeTelemetry,
            payload: vec![],
        };
        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::SubscribeTelemetry);
    }
}
