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
    /// Payload exceeds [`MAX_MESSAGE_SIZE`].
    PayloadTooLarge { size: usize, max: usize },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::InvalidMessage(msg) => write!(f, "Invalid message: {}", msg),
            ProtocolError::UnknownMessageType(b) => write!(f, "Unknown message type: {}", b),
            ProtocolError::Io(e) => write!(f, "IO error: {}", e),
            ProtocolError::Json(e) => write!(f, "JSON error: {}", e),
            ProtocolError::PayloadTooLarge { size, max } => {
                write!(f, "Payload too large: {} bytes (max {})", size, max)
            }
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

/// Maximum allowed message payload size (64 MB).
///
/// Prevents OOM from an untrusted length field: without this limit an attacker
/// can send `0xFFFFFFFF` as the length and force a 4 GB allocation.
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Protocol version for host↔guest wire format negotiation.
///
/// The version is exchanged during the Ping/Pong handshake:
///
/// - **Ping payload** (v2): `[secret: 32 B][version: 4 B LE][flags: 1 B]` (37 B)
/// - **Pong payload** (v2): `[version: 4 B LE][flags: 1 B]` (5 B)
///
/// The wire format is structurally append-only — every revision only adds
/// trailing bytes, so older parsers can read newer prefixes:
///
/// - v0 hosts sent a 32-byte Ping (no version). Parsers treat `len < 36`
///   as version 0.
/// - v1 hosts/guests sent the 4-byte version but no flags. Parsers treat
///   `len < 37` (Ping) or `len < 5` (Pong) as flags=0.
/// - v2 adds a flags byte (see [`PROTO_FLAG_SUPPORTS_MULTIPLEX`]).
///
/// **Wire-level backward compatibility is not the same as semantic
/// compatibility.** Starting in protocol v2, peers must negotiate
/// `PROTO_FLAG_SUPPORTS_MULTIPLEX` during the Ping/Pong exchange:
/// every post-handshake frame carries a 4-byte `request_id` prefix
/// that pre-multiplex peers cannot decode, so guest-agent and host-side
/// `ControlChannel` both hard-reject any peer that does not advertise
/// the flag. The previous "per-RPC reconnect fallback" path has been
/// removed. Older v0/v1 peers will fail at handshake with a clear
/// protocol-version error rather than corrupt the byte stream.
///
/// Use [`build_ping_payload`], [`parse_ping_payload`],
/// [`build_pong_payload`], and [`parse_pong_payload`] instead of hand-
/// rolling byte offsets.
pub const PROTOCOL_VERSION: u32 = 2;

/// Peer supports one long-lived multiplexed control channel per sandbox
/// (see `docs/superpowers/plans/2026-04-20-startup-milestones-b-c-d.md`
/// Lever 7). Advertised via `flags` byte in both Ping and Pong.
///
/// **Required since protocol v2.** Every post-handshake frame carries
/// a 4-byte `request_id` prefix that pre-multiplex peers cannot decode,
/// so accepting a peer without this flag would corrupt every
/// subsequent frame. Guest-agent and host-side `ControlChannel` both
/// hard-reject at handshake when the peer's flag byte does not have
/// this bit set. The flag exists only as an explicit feature negotiation
/// point so future versions can layer additional capabilities under
/// the same Ping/Pong format.
pub const PROTO_FLAG_SUPPORTS_MULTIPLEX: u8 = 0b0000_0001;

/// Builds a Ping payload with the session secret, protocol version, and
/// the caller's feature flags.
///
/// # Examples
///
/// ```
/// use void_box_protocol::{build_ping_payload, PROTO_FLAG_SUPPORTS_MULTIPLEX};
///
/// let secret = [0xABu8; 32];
/// let payload = build_ping_payload(&secret, PROTO_FLAG_SUPPORTS_MULTIPLEX);
/// assert_eq!(payload.len(), 37);
/// ```
pub fn build_ping_payload(secret: &[u8; 32], flags: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 + 4 + 1);
    buf.extend_from_slice(secret);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    buf.push(flags);
    buf
}

/// Parses a Ping payload, returning the session secret, peer's protocol
/// version, and peer's feature flags.
///
/// Returns `None` if the payload is shorter than 32 bytes (no secret).
/// Older peers may omit the version (returns 0) or flags (returns 0).
pub fn parse_ping_payload(payload: &[u8]) -> Option<(&[u8; 32], u32, u8)> {
    if payload.len() < 32 {
        return None;
    }
    let secret: &[u8; 32] = payload[..32].try_into().ok()?;
    let version = if payload.len() >= 36 {
        u32::from_le_bytes([payload[32], payload[33], payload[34], payload[35]])
    } else {
        0
    };
    let flags = if payload.len() >= 37 { payload[36] } else { 0 };
    Some((secret, version, flags))
}

/// Builds a Pong payload with the protocol version and the caller's
/// feature flags.
///
/// # Examples
///
/// ```
/// use void_box_protocol::{build_pong_payload, PROTO_FLAG_SUPPORTS_MULTIPLEX};
///
/// let payload = build_pong_payload(PROTO_FLAG_SUPPORTS_MULTIPLEX);
/// assert_eq!(payload.len(), 5);
/// ```
pub fn build_pong_payload(flags: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + 1);
    buf.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    buf.push(flags);
    buf
}

/// Parses a Pong payload, returning the peer's protocol version and
/// feature flags.
///
/// Older peers may send only the 4-byte version (returns flags=0) or
/// nothing at all (returns version=0, flags=0).
pub fn parse_pong_payload(payload: &[u8]) -> (u32, u8) {
    let version = if payload.len() >= 4 {
        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
    } else {
        0
    };
    let flags = if payload.len() >= 5 { payload[4] } else { 0 };
    (version, flags)
}

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
    /// Guest signals that it is ready for a snapshot.
    SnapshotReady = 17,
    /// Reads a file from the guest filesystem.
    ReadFile = 18,
    /// Response to ReadFile.
    ReadFileResponse = 19,
    /// Checks if a file exists and returns its size.
    FileStat = 20,
    /// Response to FileStat.
    FileStatResponse = 21,
    /// Opens a new pseudo-terminal session in the guest.
    PtyOpen = 22,
    /// Confirms that a PTY session was opened successfully or reports an error.
    PtyOpened = 23,
    /// Carries raw terminal I/O bytes for an open PTY session.
    PtyData = 24,
    /// Requests a terminal window size change for an open PTY session.
    PtyResize = 25,
    /// Requests teardown of an open PTY session.
    PtyClose = 26,
    /// Confirms that a PTY session has been closed and reports its exit code.
    PtyClosed = 27,
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
            17 => Ok(MessageType::SnapshotReady),
            18 => Ok(MessageType::ReadFile),
            19 => Ok(MessageType::ReadFileResponse),
            20 => Ok(MessageType::FileStat),
            21 => Ok(MessageType::FileStatResponse),
            22 => Ok(MessageType::PtyOpen),
            23 => Ok(MessageType::PtyOpened),
            24 => Ok(MessageType::PtyData),
            25 => Ok(MessageType::PtyResize),
            26 => Ok(MessageType::PtyClose),
            27 => Ok(MessageType::PtyClosed),
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

        if length > MAX_MESSAGE_SIZE {
            return Err(ProtocolError::PayloadTooLarge {
                size: length,
                max: MAX_MESSAGE_SIZE,
            });
        }

        let msg_type = MessageType::try_from(data[4])?;

        if data.len() < HEADER_SIZE + length {
            return Err(ProtocolError::InvalidMessage("Incomplete message".into()));
        }

        let payload = data[HEADER_SIZE..HEADER_SIZE + length].to_vec();
        Ok(Self { msg_type, payload })
    }

    /// Read a complete message from a synchronous [`std::io::Read`] stream.
    pub fn read_from_sync<R: std::io::Read + ?Sized>(
        reader: &mut R,
    ) -> Result<Self, ProtocolError> {
        let mut header = [0u8; HEADER_SIZE];
        reader.read_exact(&mut header)?;

        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;

        if length > MAX_MESSAGE_SIZE {
            return Err(ProtocolError::PayloadTooLarge {
                size: length,
                max: MAX_MESSAGE_SIZE,
            });
        }

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
#[derive(Clone, Serialize, Deserialize)]
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

/// Patterns that indicate a sensitive environment variable key.
const SENSITIVE_KEY_PATTERNS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD"];

/// Returns true if the environment variable key looks like it holds a secret.
fn is_sensitive_env_key(key: &str) -> bool {
    let upper = key.to_uppercase();
    SENSITIVE_KEY_PATTERNS
        .iter()
        .any(|pattern| upper.contains(pattern))
}

impl fmt::Debug for ExecRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_env: Vec<(String, String)> = self
            .env
            .iter()
            .map(|(k, v)| {
                if is_sensitive_env_key(k) {
                    (k.clone(), "[REDACTED]".to_string())
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect();

        f.debug_struct("ExecRequest")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("stdin", &format!("[{} bytes]", self.stdin.len()))
            .field("env", &redacted_env)
            .field("working_dir", &self.working_dir)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
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

/// Requests reading a file from the guest filesystem.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
}

/// Response to a [`ReadFileRequest`].
#[derive(Debug, Serialize, Deserialize)]
pub struct ReadFileResponse {
    pub success: bool,
    pub content: Vec<u8>,
    pub error: Option<String>,
}

/// Requests file metadata from the guest filesystem.
#[derive(Debug, Serialize, Deserialize)]
pub struct FileStatRequest {
    pub path: String,
}

/// Response to a [`FileStatRequest`].
#[derive(Debug, Serialize, Deserialize)]
pub struct FileStatResponse {
    pub exists: bool,
    pub size: Option<u64>,
    pub error: Option<String>,
}

/// Request to open a pseudo-terminal session in the guest.
///
/// The host sends this to spawn a program under a PTY with the given
/// initial terminal dimensions, environment, and working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyOpenRequest {
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
    /// Program to execute under the PTY.
    pub program: String,
    /// Arguments to the program.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the PTY process.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory for the PTY process.
    pub working_dir: Option<String>,
    /// Interactive mode relaxes resource limits (e.g. no FSIZE cap).
    #[serde(default)]
    pub interactive: bool,
}

/// Response confirming whether a PTY session was opened.
///
/// Sent by the guest after processing a [`PtyOpenRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyOpenedResponse {
    /// Whether the PTY was opened successfully.
    pub success: bool,
    /// Error message if the open failed.
    pub error: Option<String>,
}

/// Request to resize the terminal window of an open PTY session.
///
/// Sent by the host when the user's terminal dimensions change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyResizeRequest {
    /// New terminal width in columns.
    pub cols: u16,
    /// New terminal height in rows.
    pub rows: u16,
}

/// Response confirming that a PTY session has been closed.
///
/// Sent by the guest after the PTY process exits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyClosedResponse {
    /// Exit code of the PTY process.
    pub exit_code: i32,
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

/// Subscription options sent by the host with `SubscribeTelemetry`.
///
/// The `#[serde(default)]` annotations ensure backward compatibility:
/// an empty `{}` payload or old hosts sending `vec![]` will deserialize
/// with defaults (1 s interval, no kernel threads).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySubscribeRequest {
    /// Collection interval in milliseconds. Default: 1000.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    /// Include kernel threads in per-process metrics. Default: false.
    #[serde(default)]
    pub include_kernel_threads: bool,
}

fn default_interval_ms() -> u64 {
    1000
}

impl Default for TelemetrySubscribeRequest {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            include_kernel_threads: false,
        }
    }
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
        assert!(MessageType::try_from(28).is_err());
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
    fn message_payload_too_large_deserialize() {
        // Header says payload is larger than MAX_MESSAGE_SIZE
        let huge_len = (MAX_MESSAGE_SIZE + 1) as u32;
        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&huge_len.to_le_bytes());
        data[4] = 1; // ExecRequest
        let err = Message::deserialize(&data).unwrap_err();
        assert!(
            matches!(err, ProtocolError::PayloadTooLarge { .. }),
            "expected PayloadTooLarge, got: {:?}",
            err,
        );
    }

    #[test]
    fn message_payload_too_large_read_from_sync() {
        let huge_len = (MAX_MESSAGE_SIZE + 1) as u32;
        let mut wire = Vec::new();
        wire.extend_from_slice(&huge_len.to_le_bytes());
        wire.push(1); // ExecRequest
        let mut cursor = std::io::Cursor::new(wire);
        let err = Message::read_from_sync(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::PayloadTooLarge { .. }));
    }

    #[test]
    fn exec_request_debug_redacts_secrets() {
        let req = ExecRequest {
            program: "echo".to_string(),
            args: vec![],
            stdin: Vec::new(),
            env: vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                (
                    "ANTHROPIC_API_KEY".to_string(),
                    "sk-ant-secret-123".to_string(),
                ),
                ("MY_SECRET".to_string(), "hunter2".to_string()),
                ("AUTH_TOKEN".to_string(), "tok_abc".to_string()),
                ("DB_PASSWORD".to_string(), "p@ss".to_string()),
                ("NORMAL_VAR".to_string(), "visible".to_string()),
            ],
            working_dir: None,
            timeout_secs: None,
        };
        let debug_output = format!("{:?}", req);
        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("sk-ant-secret-123"));
        assert!(!debug_output.contains("hunter2"));
        assert!(!debug_output.contains("tok_abc"));
        assert!(!debug_output.contains("p@ss"));
        assert!(debug_output.contains("visible"));
        assert!(debug_output.contains("/usr/bin"));
    }

    #[test]
    fn telemetry_subscribe_request_json_round_trip() {
        let req = TelemetrySubscribeRequest {
            interval_ms: 500,
            include_kernel_threads: true,
        };
        let json = serde_json::to_vec(&req).unwrap();
        let decoded: TelemetrySubscribeRequest = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.interval_ms, 500);
        assert!(decoded.include_kernel_threads);
    }

    #[test]
    fn telemetry_subscribe_request_empty_payload_defaults() {
        // Simulates old host sending empty vec![] — deserialization should use defaults
        let decoded: TelemetrySubscribeRequest = serde_json::from_slice(b"{}").unwrap();
        assert_eq!(decoded.interval_ms, 1000);
        assert!(!decoded.include_kernel_threads);
    }

    #[test]
    fn telemetry_subscribe_request_default_trait() {
        let req = TelemetrySubscribeRequest::default();
        assert_eq!(req.interval_ms, 1000);
        assert!(!req.include_kernel_threads);
    }

    #[test]
    fn protocol_version_is_nonzero() {
        const { assert!(PROTOCOL_VERSION > 0, "PROTOCOL_VERSION must be > 0") };
    }

    #[test]
    fn ping_with_version_round_trip() {
        // Simulate a v1 Ping: 32-byte secret + 4-byte LE version
        let secret = [0xABu8; 32];
        let mut ping_payload = secret.to_vec();
        ping_payload.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        assert_eq!(ping_payload.len(), 36);

        // Parse back
        assert_eq!(&ping_payload[..32], &secret[..]);
        let ver = u32::from_le_bytes([
            ping_payload[32],
            ping_payload[33],
            ping_payload[34],
            ping_payload[35],
        ]);
        assert_eq!(ver, PROTOCOL_VERSION);
    }

    #[test]
    fn pong_with_version_round_trip() {
        // Simulate a v1 Pong: 4-byte LE version
        let pong_payload = PROTOCOL_VERSION.to_le_bytes().to_vec();
        let msg = Message {
            msg_type: MessageType::Pong,
            payload: pong_payload,
        };
        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::Pong);
        assert_eq!(decoded.payload.len(), 4);
        let ver = u32::from_le_bytes([
            decoded.payload[0],
            decoded.payload[1],
            decoded.payload[2],
            decoded.payload[3],
        ]);
        assert_eq!(ver, PROTOCOL_VERSION);
    }

    #[test]
    fn file_stat_request_round_trip() {
        let req = FileStatRequest {
            path: "/workspace/output.json".into(),
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let decoded: FileStatRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.path, "/workspace/output.json");
    }

    #[test]
    fn file_stat_response_exists() {
        let resp = FileStatResponse {
            exists: true,
            size: Some(42),
            error: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let decoded: FileStatResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded.exists);
        assert_eq!(decoded.size, Some(42));
    }

    #[test]
    fn file_stat_response_missing() {
        let resp = FileStatResponse {
            exists: false,
            size: None,
            error: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let decoded: FileStatResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!decoded.exists);
    }

    #[test]
    fn read_file_request_round_trip() {
        let req = ReadFileRequest {
            path: "/workspace/data.bin".into(),
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let decoded: ReadFileRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.path, "/workspace/data.bin");
    }

    #[test]
    fn read_file_response_success() {
        let resp = ReadFileResponse {
            success: true,
            content: b"hello".to_vec(),
            error: None,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let decoded: ReadFileResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded.success);
        assert_eq!(decoded.content, b"hello");
    }

    #[test]
    fn read_file_response_failure() {
        let resp = ReadFileResponse {
            success: false,
            content: Vec::new(),
            error: Some("not found".into()),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let decoded: ReadFileResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!decoded.success);
        assert_eq!(decoded.error.as_deref(), Some("not found"));
    }

    #[test]
    fn message_type_round_trip_new_variants() {
        assert_eq!(MessageType::try_from(18u8).unwrap(), MessageType::ReadFile);
        assert_eq!(
            MessageType::try_from(19u8).unwrap(),
            MessageType::ReadFileResponse
        );
        assert_eq!(MessageType::try_from(20u8).unwrap(), MessageType::FileStat);
        assert_eq!(
            MessageType::try_from(21u8).unwrap(),
            MessageType::FileStatResponse
        );
    }

    #[test]
    fn build_ping_payload_layout() {
        let secret = [0xABu8; 32];
        let payload = build_ping_payload(&secret, PROTO_FLAG_SUPPORTS_MULTIPLEX);
        assert_eq!(payload.len(), 37);
        assert_eq!(&payload[..32], &secret[..]);
        assert_eq!(
            u32::from_le_bytes([payload[32], payload[33], payload[34], payload[35]]),
            PROTOCOL_VERSION
        );
        assert_eq!(payload[36], PROTO_FLAG_SUPPORTS_MULTIPLEX);
    }

    #[test]
    fn parse_ping_payload_v2_round_trip() {
        let secret = [0xCDu8; 32];
        let payload = build_ping_payload(&secret, PROTO_FLAG_SUPPORTS_MULTIPLEX);
        let (parsed_secret, version, flags) = parse_ping_payload(&payload).unwrap();
        assert_eq!(parsed_secret, &secret);
        assert_eq!(version, PROTOCOL_VERSION);
        assert_eq!(flags, PROTO_FLAG_SUPPORTS_MULTIPLEX);
    }

    #[test]
    fn parse_ping_payload_v1_flags_default_zero() {
        // v1 peer: 32-byte secret + 4-byte version, no flags byte
        let secret = [0xEFu8; 32];
        let mut payload = secret.to_vec();
        payload.extend_from_slice(&1u32.to_le_bytes());
        let (parsed_secret, version, flags) = parse_ping_payload(&payload).unwrap();
        assert_eq!(parsed_secret, &secret);
        assert_eq!(version, 1);
        assert_eq!(flags, 0);
    }

    #[test]
    fn parse_ping_payload_v0_version_default_zero() {
        // v0 peer: 32-byte secret only
        let secret = [0x11u8; 32];
        let (parsed_secret, version, flags) = parse_ping_payload(&secret).unwrap();
        assert_eq!(parsed_secret, &secret);
        assert_eq!(version, 0);
        assert_eq!(flags, 0);
    }

    #[test]
    fn parse_ping_payload_too_short_returns_none() {
        assert!(parse_ping_payload(&[0u8; 31]).is_none());
        assert!(parse_ping_payload(&[]).is_none());
    }

    #[test]
    fn build_pong_payload_layout() {
        let payload = build_pong_payload(PROTO_FLAG_SUPPORTS_MULTIPLEX);
        assert_eq!(payload.len(), 5);
        assert_eq!(
            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
            PROTOCOL_VERSION
        );
        assert_eq!(payload[4], PROTO_FLAG_SUPPORTS_MULTIPLEX);
    }

    #[test]
    fn parse_pong_payload_v2_round_trip() {
        let payload = build_pong_payload(PROTO_FLAG_SUPPORTS_MULTIPLEX);
        let (version, flags) = parse_pong_payload(&payload);
        assert_eq!(version, PROTOCOL_VERSION);
        assert_eq!(flags, PROTO_FLAG_SUPPORTS_MULTIPLEX);
    }

    #[test]
    fn parse_pong_payload_v1_flags_default_zero() {
        // v1 peer: 4-byte version only
        let payload = 1u32.to_le_bytes().to_vec();
        let (version, flags) = parse_pong_payload(&payload);
        assert_eq!(version, 1);
        assert_eq!(flags, 0);
    }

    #[test]
    fn parse_pong_payload_v0_defaults_zero() {
        let (version, flags) = parse_pong_payload(&[]);
        assert_eq!(version, 0);
        assert_eq!(flags, 0);
    }

    #[test]
    fn legacy_ping_32_bytes_still_valid() {
        // Old hosts send only 32 bytes (no version field).
        // The parser should treat this as version 0.
        let secret = [0xCDu8; 32];
        let ping_payload = secret.to_vec();
        assert_eq!(ping_payload.len(), 32);

        // Version detection: if len < 36, treat as version 0
        let ver = if ping_payload.len() >= 36 {
            u32::from_le_bytes([
                ping_payload[32],
                ping_payload[33],
                ping_payload[34],
                ping_payload[35],
            ])
        } else {
            0
        };
        assert_eq!(ver, 0);
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

    #[test]
    fn pty_message_types_round_trip() {
        for &(byte, expected) in &[
            (22u8, MessageType::PtyOpen),
            (23, MessageType::PtyOpened),
            (24, MessageType::PtyData),
            (25, MessageType::PtyResize),
            (26, MessageType::PtyClose),
            (27, MessageType::PtyClosed),
        ] {
            assert_eq!(MessageType::try_from(byte).unwrap(), expected);
        }
    }

    #[test]
    fn pty_open_request_json_round_trip() {
        let req = PtyOpenRequest {
            cols: 80,
            rows: 24,
            program: "/bin/bash".to_string(),
            args: vec!["-l".to_string()],
            env: vec![("TERM".to_string(), "xterm-256color".to_string())],
            working_dir: Some("/home/user".to_string()),
            interactive: false,
        };
        let json = serde_json::to_vec(&req).unwrap();
        let decoded: PtyOpenRequest = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.cols, 80);
        assert_eq!(decoded.rows, 24);
        assert_eq!(decoded.program, "/bin/bash");
        assert_eq!(decoded.args, vec!["-l"]);
        assert_eq!(decoded.env, vec![("TERM".into(), "xterm-256color".into())]);
        assert_eq!(decoded.working_dir.as_deref(), Some("/home/user"));

        let minimal: PtyOpenRequest =
            serde_json::from_str(r#"{"cols":120,"rows":40,"program":"sh"}"#).unwrap();
        assert!(minimal.args.is_empty());
        assert!(minimal.env.is_empty());
        assert!(minimal.working_dir.is_none());
    }

    #[test]
    fn pty_opened_response_json_round_trip() {
        let ok = PtyOpenedResponse {
            success: true,
            error: None,
        };
        let json = serde_json::to_vec(&ok).unwrap();
        let decoded: PtyOpenedResponse = serde_json::from_slice(&json).unwrap();
        assert!(decoded.success);
        assert!(decoded.error.is_none());

        let fail = PtyOpenedResponse {
            success: false,
            error: Some("no such program".to_string()),
        };
        let json = serde_json::to_vec(&fail).unwrap();
        let decoded: PtyOpenedResponse = serde_json::from_slice(&json).unwrap();
        assert!(!decoded.success);
        assert_eq!(decoded.error.as_deref(), Some("no such program"));
    }

    #[test]
    fn pty_resize_request_json_round_trip() {
        let req = PtyResizeRequest {
            cols: 132,
            rows: 43,
        };
        let json = serde_json::to_vec(&req).unwrap();
        let decoded: PtyResizeRequest = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.cols, 132);
        assert_eq!(decoded.rows, 43);
    }

    #[test]
    fn pty_closed_response_json_round_trip() {
        let resp = PtyClosedResponse { exit_code: 0 };
        let json = serde_json::to_vec(&resp).unwrap();
        let decoded: PtyClosedResponse = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.exit_code, 0);

        let resp = PtyClosedResponse { exit_code: 137 };
        let json = serde_json::to_vec(&resp).unwrap();
        let decoded: PtyClosedResponse = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.exit_code, 137);
    }

    #[test]
    fn pty_data_uses_raw_bytes() {
        let raw = b"\x1b[31mhello\x1b[0m\r\n";
        let msg = Message {
            msg_type: MessageType::PtyData,
            payload: raw.to_vec(),
        };
        let bytes = msg.serialize();
        let decoded = Message::deserialize(&bytes).unwrap();
        assert_eq!(decoded.msg_type, MessageType::PtyData);
        assert_eq!(decoded.payload, raw);
    }
}
