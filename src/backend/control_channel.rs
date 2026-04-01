//! Transport-agnostic control channel for host↔guest communication.
//!
//! This module extracts the protocol logic from `devices::virtio_vsock::VsockDevice`
//! into a reusable `ControlChannel` that works over any `GuestStream`.
//!
//! The only platform-specific part is the *connector* closure that produces
//! a `GuestStream`:
//! - **Linux/KVM**: `AF_VSOCK` socket → `VsockStream`
//! - **macOS/VZ**: `VZVirtioSocketConnection.fileDescriptor()` → fd wrapper
//!
//! ## I/O model
//!
//! All protocol I/O (connect, handshake, request/response) is **synchronous**
//! (`std::io::Read`/`Write`).  Each public method is `async fn` but offloads
//! the blocking work to [`tokio::task::spawn_blocking`].  See the "Control
//! channel I/O model" section in `AGENTS.md` for the design rationale.
//!
//! The pattern for every public method:
//! 1. Serialize the outgoing message on the caller's async task (cheap).
//! 2. Clone the `Arc` fields needed by the blocking closure.
//! 3. Inside `spawn_blocking`: connect → handshake → send → read loop.

use std::io::{self, Read, Write};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use void_box_protocol::ProtocolError;

use crate::guest::protocol::{
    ExecOutputChunk, ExecRequest, ExecResponse, FileStatRequest, FileStatResponse, Message,
    MessageType, MkdirPRequest, MkdirPResponse, PtyOpenRequest, ReadFileRequest, ReadFileResponse,
    TelemetryBatch, TelemetrySubscribeRequest, WriteFileRequest, WriteFileResponse,
};
use crate::{Error, Result};

/// vsock port used by the guest agent.
pub const GUEST_AGENT_PORT: u32 = 1234;

/// A stream to the guest agent that supports `Read`, `Write`, and timeout control.
///
/// Both AF_VSOCK sockets (Linux) and VZ socket connections (macOS) expose
/// raw file descriptors, so this trait is trivially implementable on both.
pub trait GuestStream: Read + Write + Send {
    /// Set the read timeout. `None` means blocking (no timeout).
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// Returns the underlying file descriptor for this stream.
    fn as_raw_fd(&self) -> RawFd;
}

/// A function that creates a new connection to the guest agent.
///
/// Called each time a new request needs a fresh connection.
/// `Arc` (not `Box`) so it can be cloned into `spawn_blocking` closures.
pub type GuestConnector = Arc<dyn Fn() -> Result<Box<dyn GuestStream>> + Send + Sync>;

/// Transport-agnostic control channel for guest communication.
///
/// Encapsulates the Ping/Pong handshake, exec requests, file writes,
/// and telemetry subscriptions. The actual transport is provided by
/// the `connector` closure.
pub struct ControlChannel {
    /// Factory for creating new guest connections.
    connector: GuestConnector,
    /// 32-byte session secret for authentication.
    session_secret: [u8; 32],
    /// Whether the initial boot wait has been applied.
    boot_wait_done: Arc<AtomicBool>,
}

impl ControlChannel {
    /// Create a new control channel with the given connector and session secret.
    pub fn new(connector: GuestConnector, session_secret: [u8; 32]) -> Self {
        Self {
            connector,
            session_secret,
            boot_wait_done: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Send an exec request and wait for the response.
    ///
    /// Performs a connect+handshake, sends the request, then reads messages
    /// in a loop (discarding streaming chunks) until the final ExecResponse.
    pub async fn send_exec_request(&self, request: &ExecRequest) -> Result<ExecResponse> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let timeout = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1200));
        let msg_bytes = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "exec",
            )?;

            let _ = stream.set_read_timeout(Some(timeout));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

            debug!("control_channel: sent ExecRequest, waiting for ExecResponse");

            loop {
                let msg = Message::read_from_sync(&mut *stream)?;
                match msg.msg_type {
                    MessageType::ExecOutputChunk => continue,
                    MessageType::ExecResponse => {
                        let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                        debug!(
                            "control_channel: ExecResponse received exit_code={}",
                            response.exit_code
                        );
                        return Ok(response);
                    }
                    other => {
                        return Err(Error::Guest(format!(
                            "Unexpected response type: {:?}",
                            other
                        )));
                    }
                }
            }
        })
        .await
        .map_err(|e| Error::Guest(format!("exec task panicked: {e}")))?
    }

    /// Send an exec request and stream output chunks as they arrive.
    pub async fn send_exec_request_streaming<F>(
        &self,
        request: &ExecRequest,
        on_chunk: F,
    ) -> Result<ExecResponse>
    where
        F: FnMut(ExecOutputChunk) + Send + 'static,
    {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let timeout = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1200));
        let msg_bytes = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut on_chunk = on_chunk;
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "exec-streaming",
            )?;

            let _ = stream.set_read_timeout(Some(timeout));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

            debug!(
                "control_channel: sent ExecRequest (streaming), waiting for chunks + ExecResponse"
            );

            loop {
                let msg = Message::read_from_sync(&mut *stream)?;
                match msg.msg_type {
                    MessageType::ExecOutputChunk => {
                        match serde_json::from_slice::<ExecOutputChunk>(&msg.payload) {
                            Ok(chunk) => on_chunk(chunk),
                            Err(e) => warn!(
                                "Malformed ExecOutputChunk ({}B payload): {}",
                                msg.payload.len(),
                                e
                            ),
                        }
                    }
                    MessageType::ExecResponse => {
                        let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                        debug!(
                            "control_channel: ExecResponse received (streaming) exit_code={}",
                            response.exit_code
                        );
                        return Ok(response);
                    }
                    other => {
                        warn!("Unexpected message type during streaming exec: {:?}", other);
                    }
                }
            }
        })
        .await
        .map_err(|e| Error::Guest(format!("streaming exec task panicked: {e}")))?
    }

    /// Async-friendly streaming exec: chunks are sent via the mpsc channel.
    ///
    /// Connects, sends the request, then reads output in a blocking task.
    pub async fn send_exec_request_streaming_async(
        &self,
        request: &ExecRequest,
        chunk_tx: tokio::sync::mpsc::Sender<ExecOutputChunk>,
    ) -> Result<ExecResponse> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let timeout = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1200));
        let msg_bytes = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "exec-streaming",
            )?;

            let _ = stream.set_read_timeout(Some(timeout));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

            debug!(
                "control_channel: sent ExecRequest (streaming), waiting for chunks + ExecResponse"
            );

            loop {
                let msg = Message::read_from_sync(&mut *stream)?;
                match msg.msg_type {
                    MessageType::ExecOutputChunk => {
                        match serde_json::from_slice::<ExecOutputChunk>(&msg.payload) {
                            Ok(chunk) => {
                                let _ = chunk_tx.blocking_send(chunk);
                            }
                            Err(e) => warn!(
                                "Malformed ExecOutputChunk ({}B payload): {}",
                                msg.payload.len(),
                                e
                            ),
                        }
                    }
                    MessageType::ExecResponse => {
                        let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                        debug!(
                            "control_channel: ExecResponse received (streaming) exit_code={}",
                            response.exit_code
                        );
                        return Ok(response);
                    }
                    other => {
                        warn!("Unexpected message type during streaming exec: {:?}", other);
                    }
                }
            }
        })
        .await
        .map_err(|e| Error::Guest(format!("streaming task panicked: {e}")))?
    }

    /// Write a file to the guest filesystem using the native WriteFile protocol.
    pub async fn send_write_file(&self, path: &str, content: &[u8]) -> Result<WriteFileResponse> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let msg_bytes = Message {
            msg_type: MessageType::WriteFile,
            payload: serde_json::to_vec(&WriteFileRequest {
                path: path.to_string(),
                content: content.to_vec(),
                create_parents: true,
            })?,
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "write-file",
            )?;

            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send WriteFile: {}", e)))?;

            let response_msg = Message::read_from_sync(&mut *stream)?;
            if response_msg.msg_type != MessageType::WriteFileResponse {
                return Err(Error::Guest(format!(
                    "Unexpected response type for WriteFile: {:?}",
                    response_msg.msg_type
                )));
            }

            let response: WriteFileResponse = serde_json::from_slice(&response_msg.payload)?;
            Ok(response)
        })
        .await
        .map_err(|e| Error::Guest(format!("write_file task panicked: {e}")))?
    }

    /// Create directories in the guest filesystem (mkdir -p).
    pub async fn send_mkdir_p(&self, path: &str) -> Result<MkdirPResponse> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let msg_bytes = Message {
            msg_type: MessageType::MkdirP,
            payload: serde_json::to_vec(&MkdirPRequest {
                path: path.to_string(),
            })?,
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "mkdir-p",
            )?;

            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send MkdirP: {}", e)))?;

            let response_msg = Message::read_from_sync(&mut *stream)?;
            if response_msg.msg_type != MessageType::MkdirPResponse {
                return Err(Error::Guest(format!(
                    "Unexpected response type for MkdirP: {:?}",
                    response_msg.msg_type
                )));
            }

            let response: MkdirPResponse = serde_json::from_slice(&response_msg.payload)?;
            Ok(response)
        })
        .await
        .map_err(|e| Error::Guest(format!("mkdir_p task panicked: {e}")))?
    }

    /// Checks if a file exists in the guest filesystem.
    pub async fn send_file_stat(&self, path: &str) -> Result<FileStatResponse> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let msg_bytes = Message {
            msg_type: MessageType::FileStat,
            payload: serde_json::to_vec(&FileStatRequest {
                path: path.to_string(),
            })?,
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "file-stat",
            )?;

            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send FileStat: {e}")))?;

            let response_msg = Message::read_from_sync(&mut *stream)?;
            if response_msg.msg_type != MessageType::FileStatResponse {
                return Err(Error::Guest(format!(
                    "Unexpected response type for FileStat: {:?}",
                    response_msg.msg_type
                )));
            }

            let response: FileStatResponse = serde_json::from_slice(&response_msg.payload)?;
            Ok(response)
        })
        .await
        .map_err(|e| Error::Guest(format!("file_stat task panicked: {e}")))?
    }

    /// Reads a file from the guest filesystem.
    pub async fn send_read_file(&self, path: &str) -> Result<ReadFileResponse> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let msg_bytes = Message {
            msg_type: MessageType::ReadFile,
            payload: serde_json::to_vec(&ReadFileRequest {
                path: path.to_string(),
            })?,
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "read-file",
            )?;

            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send ReadFile: {e}")))?;

            let response_msg = Message::read_from_sync(&mut *stream)?;
            if response_msg.msg_type != MessageType::ReadFileResponse {
                return Err(Error::Guest(format!(
                    "Unexpected response type for ReadFile: {:?}",
                    response_msg.msg_type
                )));
            }

            let response: ReadFileResponse = serde_json::from_slice(&response_msg.payload)?;
            Ok(response)
        })
        .await
        .map_err(|e| Error::Guest(format!("read_file task panicked: {e}")))?
    }

    /// Open a persistent telemetry subscription to the guest agent.
    pub async fn subscribe_telemetry<F>(
        &self,
        opts: &TelemetrySubscribeRequest,
        on_batch: F,
    ) -> Result<()>
    where
        F: FnMut(TelemetryBatch) + Send + 'static,
    {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let sub_bytes = Message {
            msg_type: MessageType::SubscribeTelemetry,
            payload: serde_json::to_vec(opts).unwrap_or_default(),
        }
        .serialize();
        let interval_ms = opts.interval_ms;

        tokio::task::spawn_blocking(move || {
            let mut on_batch = on_batch;
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(5),
                "telemetry-subscribe",
            )?;

            stream
                .write_all(&sub_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send SubscribeTelemetry: {}", e)))?;

            info!("Telemetry subscription active (interval={}ms)", interval_ms);

            let read_timeout_ms = interval_ms.max(1000) * 5;
            let _ = stream.set_read_timeout(Some(Duration::from_millis(read_timeout_ms)));

            loop {
                let msg = match Message::read_from_sync(&mut *stream) {
                    Ok(m) => m,
                    Err(e) => {
                        // Timeouts and connection resets are expected when the
                        // VM shuts down or the subscription interval elapses.
                        if is_expected_stream_end(&e) {
                            info!("Telemetry subscription ended: {}", e);
                        } else {
                            warn!("Telemetry subscription ended unexpectedly: {}", e);
                        }
                        return Ok(());
                    }
                };

                if msg.msg_type != MessageType::TelemetryData {
                    warn!(
                        "Unexpected message type in telemetry stream: {:?}",
                        msg.msg_type
                    );
                    continue;
                }

                match serde_json::from_slice::<TelemetryBatch>(&msg.payload) {
                    Ok(batch) => on_batch(batch),
                    Err(e) => {
                        warn!("Failed to parse TelemetryBatch: {}", e);
                    }
                }
            }
        })
        .await
        .map_err(|e| Error::Guest(format!("telemetry task panicked: {e}")))?
    }

    /// Wait for the guest to signal snapshot readiness.
    ///
    /// Connects and sends a `SnapshotReady` message, then waits for the
    /// guest to reply with `SnapshotReady`. Returns `Ok(())` on success.
    pub async fn wait_for_snapshot_ready(&self, timeout: Duration) -> Result<()> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let msg_bytes = Message {
            msg_type: MessageType::SnapshotReady,
            payload: Vec::new(),
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let mut stream = connect_with_handshake_sync(
                &connector,
                &session_secret,
                &boot_wait_done,
                Duration::from_secs(3),
                "snapshot-ready",
            )?;

            let _ = stream.set_read_timeout(Some(timeout));
            stream
                .write_all(&msg_bytes)
                .map_err(|e| Error::Guest(format!("Failed to send SnapshotReady: {}", e)))?;

            let response_msg = Message::read_from_sync(&mut *stream)?;
            if response_msg.msg_type != MessageType::SnapshotReady {
                return Err(Error::Guest(format!(
                    "Unexpected response for SnapshotReady: {:?}",
                    response_msg.msg_type
                )));
            }

            debug!("control_channel: guest confirmed SnapshotReady");
            Ok(())
        })
        .await
        .map_err(|e| Error::Guest(format!("snapshot_ready task panicked: {e}")))?
    }

    /// Opens a PTY session on the guest, returning a [`PtySession`] that owns the connection.
    pub async fn open_pty(
        &self,
        request: PtyOpenRequest,
    ) -> Result<super::pty_session::PtySession> {
        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);
        tokio::task::spawn_blocking(move || {
            super::pty_session::PtySession::open(
                &connector,
                &session_secret,
                &boot_wait_done,
                &request,
            )
        })
        .await
        .map_err(|e| Error::Guest(format!("pty task panicked: {e}")))?
    }
}

/// Connect to the guest agent and perform a Ping/Pong handshake.
///
/// Fully synchronous — intended to be called from `spawn_blocking` closures.
/// Uses [`std::thread::sleep`] for backoff delays (not `tokio::time::sleep`).
pub(crate) fn connect_with_handshake_sync(
    connector: &GuestConnector,
    session_secret: &[u8; 32],
    boot_wait_done: &AtomicBool,
    handshake_timeout: Duration,
    context: &str,
) -> Result<Box<dyn GuestStream>> {
    // Wait for guest kernel boot once per ControlChannel lifetime.
    if boot_wait_done
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        std::thread::sleep(Duration::from_secs(4));
    }

    let mut delay = Duration::from_millis(100);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut attempt: u32 = 0;

    loop {
        if Instant::now() >= deadline {
            warn!(
                "control_channel[{context}]: deadline reached after {} connect/handshake attempts",
                attempt
            );
            return Err(Error::Guest(
                "control_channel: deadline reached (connect or handshake)".into(),
            ));
        }

        attempt += 1;

        let mut s = match connector() {
            Ok(stream) => {
                debug!("control_channel[{context}]: attempt {} connect OK", attempt);
                stream
            }
            Err(e) => {
                debug!(
                    "control_channel[{context}]: attempt {} connect failed: {} (retry in {:?})",
                    attempt, e, delay
                );
                std::thread::sleep(delay);
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                continue;
            }
        };

        // Handshake: Ping -> Pong
        if let Err(e) = s.set_read_timeout(Some(handshake_timeout)) {
            debug!(
                "control_channel[{context}]: attempt {} set_read_timeout failed: {}",
                attempt, e
            );
            std::thread::sleep(delay);
            delay = std::cmp::min(delay * 2, Duration::from_secs(2));
            continue;
        }

        // Build Ping payload: [secret: 32 bytes][version: 4 bytes LE]
        let mut ping_payload = session_secret.to_vec();
        ping_payload.extend_from_slice(&crate::guest::protocol::PROTOCOL_VERSION.to_le_bytes());
        let ping_msg = Message {
            msg_type: MessageType::Ping,
            payload: ping_payload,
        };
        if s.write_all(&ping_msg.serialize()).is_err() {
            debug!(
                "control_channel[{context}]: attempt {} failed to send Ping",
                attempt
            );
            std::thread::sleep(delay);
            delay = std::cmp::min(delay * 2, Duration::from_secs(2));
            continue;
        }
        match Message::read_from_sync(&mut *s) {
            Ok(msg) if msg.msg_type == MessageType::Pong => {
                // Parse optional protocol version from Pong payload.
                let peer_version = if msg.payload.len() >= 4 {
                    u32::from_le_bytes([
                        msg.payload[0],
                        msg.payload[1],
                        msg.payload[2],
                        msg.payload[3],
                    ])
                } else {
                    0 // legacy guest, no version in Pong
                };
                debug!(
                    "control_channel[{context}]: handshake OK (peer_version={})",
                    peer_version
                );
                return Ok(s);
            }
            Ok(msg) => {
                debug!(
                    "control_channel[{context}]: attempt {} unexpected handshake message: {:?}",
                    attempt, msg.msg_type
                );
                std::thread::sleep(delay);
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
            }
            Err(e) => {
                debug!(
                    "control_channel[{context}]: attempt {} handshake read failed: {}",
                    attempt, e
                );
                std::thread::sleep(delay);
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
            }
        }
    }
}

/// Check whether a protocol error represents an expected stream termination.
///
/// Timeouts, connection resets, and EOF are normal when the VM shuts down
/// or a long-lived subscription outlives the guest. Anything else (e.g.
/// protocol parse errors) is unexpected and should be logged at `warn`.
fn is_expected_stream_end(err: &ProtocolError) -> bool {
    match err {
        ProtocolError::Io(io_err) => matches!(
            io_err.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        ),
        // InvalidMessage with "end of stream" is a clean EOF.
        ProtocolError::InvalidMessage(msg) => msg.contains("end of stream"),
        _ => false,
    }
}
