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
//! All protocol methods (handshake, exec, write_file, etc.) are identical
//! regardless of transport.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::guest::protocol::{
    ExecOutputChunk, ExecRequest, ExecResponse, Message, MessageType, MkdirPRequest,
    MkdirPResponse, TelemetryBatch, TelemetrySubscribeRequest, WriteFileRequest, WriteFileResponse,
};
use crate::{Error, Result};

/// A stream to the guest agent that supports `Read`, `Write`, and timeout control.
///
/// Both AF_VSOCK sockets (Linux) and VZ socket connections (macOS) expose
/// raw file descriptors, so this trait is trivially implementable on both.
pub trait GuestStream: Read + Write + Send {
    /// Set the read timeout. `None` means blocking (no timeout).
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

/// A function that creates a new connection to the guest agent.
///
/// Called each time a new request needs a fresh connection.
pub type GuestConnector = Box<dyn Fn() -> Result<Box<dyn GuestStream>> + Send + Sync>;

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
    boot_wait_done: AtomicBool,
}

impl ControlChannel {
    /// Create a new control channel with the given connector and session secret.
    pub fn new(connector: GuestConnector, session_secret: [u8; 32]) -> Self {
        Self {
            connector,
            session_secret,
            boot_wait_done: AtomicBool::new(false),
        }
    }

    /// Send an exec request and wait for the response.
    ///
    /// Performs a connect+handshake, sends the request, then reads messages
    /// in a loop (discarding streaming chunks) until the final ExecResponse.
    pub async fn send_exec_request(&self, request: &ExecRequest) -> Result<ExecResponse> {
        let mut stream = self
            .connect_with_handshake(Duration::from_secs(3), "exec")
            .await?;

        let timeout = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1200));
        let _ = stream.set_read_timeout(Some(timeout));

        let message = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        };
        stream
            .write_all(&message.serialize())
            .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

        info!("control_channel: sent ExecRequest, waiting for ExecResponse");

        loop {
            let msg = Message::read_from_sync(&mut *stream)?;
            match msg.msg_type {
                MessageType::ExecOutputChunk => continue,
                MessageType::ExecResponse => {
                    let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                    info!(
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
    }

    /// Send an exec request and stream output chunks as they arrive.
    pub async fn send_exec_request_streaming<F>(
        &self,
        request: &ExecRequest,
        mut on_chunk: F,
    ) -> Result<ExecResponse>
    where
        F: FnMut(ExecOutputChunk),
    {
        let mut stream = self
            .connect_with_handshake(Duration::from_secs(3), "exec-streaming")
            .await?;

        let timeout = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1200));
        let _ = stream.set_read_timeout(Some(timeout));

        let message = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        };
        stream
            .write_all(&message.serialize())
            .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

        info!("control_channel: sent ExecRequest (streaming), waiting for chunks + ExecResponse");

        loop {
            let msg = Message::read_from_sync(&mut *stream)?;
            match msg.msg_type {
                MessageType::ExecOutputChunk => {
                    if let Ok(chunk) = serde_json::from_slice::<ExecOutputChunk>(&msg.payload) {
                        on_chunk(chunk);
                    }
                }
                MessageType::ExecResponse => {
                    let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                    info!(
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
    }

    /// Async-friendly streaming exec: connects asynchronously, then moves the
    /// synchronous read loop to a blocking task. Chunks are sent via the mpsc
    /// channel directly.
    pub async fn send_exec_request_streaming_async(
        &self,
        request: &ExecRequest,
        chunk_tx: tokio::sync::mpsc::Sender<ExecOutputChunk>,
    ) -> Result<ExecResponse> {
        let mut stream = self
            .connect_with_handshake(Duration::from_secs(3), "exec-streaming")
            .await?;

        let timeout = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(1200));
        let _ = stream.set_read_timeout(Some(timeout));

        let message = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        };
        stream
            .write_all(&message.serialize())
            .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

        info!("control_channel: sent ExecRequest (streaming), waiting for chunks + ExecResponse");

        tokio::task::spawn_blocking(move || loop {
            let msg = Message::read_from_sync(&mut *stream)?;
            match msg.msg_type {
                MessageType::ExecOutputChunk => {
                    if let Ok(chunk) = serde_json::from_slice::<ExecOutputChunk>(&msg.payload) {
                        let _ = chunk_tx.blocking_send(chunk);
                    }
                }
                MessageType::ExecResponse => {
                    let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                    info!(
                        "control_channel: ExecResponse received (streaming) exit_code={}",
                        response.exit_code
                    );
                    return Ok(response);
                }
                other => {
                    warn!("Unexpected message type during streaming exec: {:?}", other);
                }
            }
        })
        .await
        .map_err(|e| Error::Guest(format!("Streaming task panicked: {}", e)))?
    }

    /// Write a file to the guest filesystem using the native WriteFile protocol.
    pub async fn send_write_file(&self, path: &str, content: &[u8]) -> Result<WriteFileResponse> {
        let request = WriteFileRequest {
            path: path.to_string(),
            content: content.to_vec(),
            create_parents: true,
        };

        let mut stream = self
            .connect_with_handshake(Duration::from_secs(3), "write-file")
            .await?;

        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));

        let message = Message {
            msg_type: MessageType::WriteFile,
            payload: serde_json::to_vec(&request)?,
        };
        stream
            .write_all(&message.serialize())
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
    }

    /// Create directories in the guest filesystem (mkdir -p).
    pub async fn send_mkdir_p(&self, path: &str) -> Result<MkdirPResponse> {
        let request = MkdirPRequest {
            path: path.to_string(),
        };

        let mut stream = self
            .connect_with_handshake(Duration::from_secs(3), "mkdir-p")
            .await?;

        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));

        let message = Message {
            msg_type: MessageType::MkdirP,
            payload: serde_json::to_vec(&request)?,
        };
        stream
            .write_all(&message.serialize())
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
    }

    /// Open a persistent telemetry subscription to the guest agent.
    pub async fn subscribe_telemetry<F>(
        &self,
        opts: &TelemetrySubscribeRequest,
        mut on_batch: F,
    ) -> Result<()>
    where
        F: FnMut(TelemetryBatch) + Send + 'static,
    {
        let mut stream = self
            .connect_with_handshake(Duration::from_secs(5), "telemetry-subscribe")
            .await?;

        let sub_msg = Message {
            msg_type: MessageType::SubscribeTelemetry,
            payload: serde_json::to_vec(opts).unwrap_or_default(),
        };
        stream
            .write_all(&sub_msg.serialize())
            .map_err(|e| Error::Guest(format!("Failed to send SubscribeTelemetry: {}", e)))?;

        info!(
            "Telemetry subscription active (interval={}ms)",
            opts.interval_ms
        );

        let read_timeout_ms = opts.interval_ms.max(1000) * 5;
        let _ = stream.set_read_timeout(Some(Duration::from_millis(read_timeout_ms)));

        loop {
            let msg = match Message::read_from_sync(&mut *stream) {
                Ok(m) => m,
                Err(e) => {
                    info!("Telemetry subscription ended: {}", e);
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
    }

    /// Connect to the guest agent and perform a Ping/Pong handshake.
    async fn connect_with_handshake(
        &self,
        handshake_timeout: Duration,
        context: &str,
    ) -> Result<Box<dyn GuestStream>> {
        // Wait for guest kernel boot once per ControlChannel lifetime.
        if self
            .boot_wait_done
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            tokio::time::sleep(Duration::from_secs(4)).await;
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

            let mut s = match (self.connector)() {
                Ok(stream) => {
                    debug!("control_channel[{context}]: attempt {} connect OK", attempt);
                    stream
                }
                Err(e) => {
                    debug!(
                        "control_channel[{context}]: attempt {} connect failed: {} (retry in {:?})",
                        attempt, e, delay
                    );
                    tokio::time::sleep(delay).await;
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
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                continue;
            }

            // Build Ping payload: [secret: 32 bytes][version: 4 bytes LE]
            let mut ping_payload = self.session_secret.to_vec();
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
                tokio::time::sleep(delay).await;
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
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                }
                Err(e) => {
                    debug!(
                        "control_channel[{context}]: attempt {} handshake read failed: {}",
                        attempt, e
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                }
            }
        }
    }
}
