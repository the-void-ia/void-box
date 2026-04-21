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
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info, warn};

use crate::backend::multiplex::{FrameSender, MultiplexChannel, Terminator};
use crate::guest::protocol::{
    ExecOutputChunk, ExecRequest, ExecResponse, FileStatRequest, FileStatResponse, Message,
    MessageType, MkdirPRequest, MkdirPResponse, PtyOpenRequest, ReadFileRequest, ReadFileResponse,
    TelemetryBatch, TelemetrySubscribeRequest, WriteFileRequest, WriteFileResponse,
};
use crate::{Error, Result};

/// Initial per-attempt read timeout for the handshake Pong.
///
/// The handshake runs exactly once per sandbox — on first RPC or when
/// the multiplex channel is reconstructed after death. Because there
/// is no per-RPC reconnect, the old 5 ms / 150 ms tradeoff collapses
/// into a single one-shot cost.
///
/// We still want the warm path (guest-agent already bound) to converge
/// in zero retries and the cold path (guest booting, first Ping takes
/// longer than the first Pong read) to succeed without 30 seconds of
/// backoff. Starting at 5 ms and doubling up to
/// [`MAX_HANDSHAKE_READ_TIMEOUT`] on each retry gives both: warm
/// finishes on attempt 1 with a 5 ms probe, cold converges within a
/// handful of attempts as the timeout grows.
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_millis(5);

/// Cold-boot wait applied once per [`ControlChannel`] before the first
/// connect attempt.
///
/// Older host-side code slept 4 seconds unconditionally; profiling
/// showed most guests are ready in 200–800 ms, so 250 ms matches the
/// common case while still giving the kernel vhost-vsock driver time
/// to see the guest's virtio-vsock device come up. Restored channels
/// skip this wait entirely because the guest-agent is already live.
const BOOT_WAIT: Duration = Duration::from_millis(250);

/// Upper bound for the exponential per-attempt handshake read timeout.
///
/// 150 ms is the ceiling validated against agent workloads in the
/// Milestone A plan (see 2026-04-20-startup-milestones-b-c-d.md) — long
/// enough to absorb the userspace vsock worker's queueing under cold
/// boot, short enough that the handshake loop exits quickly.
const MAX_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_millis(150);

/// vsock port used by the guest agent.
pub const GUEST_AGENT_PORT: u32 = 1234;

/// Default read timeout for exec responses when the caller does not specify one.
///
/// LLM inference (especially with local models via Ollama on CPU) can take
/// 10+ minutes per turn for complex prompts with tool definitions.
const DEFAULT_EXEC_READ_TIMEOUT: Duration = Duration::from_secs(1200);

/// Resolve the read timeout for an exec request.
///
/// Service mode passes `Some(0)` to mean "wait forever" (no timeout). Any other
/// `Some(n)` is taken literally; `None` falls back to [`DEFAULT_EXEC_READ_TIMEOUT`].
/// Returning `None` instructs [`GuestStream::set_read_timeout`] to disable the
/// timeout entirely (blocking reads), instead of installing a zero-second timeout
/// that some socket impls reject as `EINVAL` or interpret as non-blocking.
fn resolve_exec_read_timeout(timeout_secs: Option<u64>) -> Option<Duration> {
    match timeout_secs {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(DEFAULT_EXEC_READ_TIMEOUT),
    }
}

/// A stream to the guest agent that supports `Read`, `Write`, and timeout control.
///
/// Both AF_VSOCK sockets (Linux) and VZ socket connections (macOS) expose
/// raw file descriptors, so this trait is trivially implementable on both.
pub trait GuestStream: Read + Write + Send {
    /// Sets the read timeout. `None` means blocking (no timeout).
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// Returns the underlying file descriptor for this stream.
    fn as_raw_fd(&self) -> RawFd;

    /// Duplicates the underlying file descriptor and returns a new boxed stream.
    ///
    /// The returned stream shares the same underlying socket so read/write
    /// from either half operate on the same guest connection. This lets the
    /// multiplex channel put the reader on a dedicated thread while the
    /// writer is shared across async RPC callers.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if the `dup(2)` syscall fails.
    fn try_clone_box(&self) -> io::Result<Box<dyn GuestStream>>;
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
///
/// All RPCs — `exec`, `write_file`, `mkdir_p`, `file_stat`, `read_file`,
/// `telemetry`, `snapshot_ready` — route through a single persistent
/// [`MultiplexChannel`] that is lazily established on first use and
/// reconstructed if it dies. The guest must advertise
/// [`PROTO_FLAG_SUPPORTS_MULTIPLEX`] during the handshake or channel
/// establishment fails.
///
/// PTY sessions open their own dedicated connection (one connection per
/// interactive shell) but that connection's framing is identical: every
/// message carries an in-payload request_id.
///
/// [`PROTO_FLAG_SUPPORTS_MULTIPLEX`]: void_box_protocol::PROTO_FLAG_SUPPORTS_MULTIPLEX
pub struct ControlChannel {
    /// Factory for creating new guest connections.
    connector: GuestConnector,
    /// 32-byte session secret for authentication.
    session_secret: [u8; 32],
    /// Whether the initial boot wait has been applied.
    boot_wait_done: Arc<AtomicBool>,
    /// Lazily-established multiplex channel. Re-established on death.
    channel: Arc<AsyncMutex<Option<MultiplexChannel>>>,
}

impl ControlChannel {
    /// Creates a new control channel with the given connector and session secret.
    pub fn new(connector: GuestConnector, session_secret: [u8; 32]) -> Self {
        Self {
            connector,
            session_secret,
            boot_wait_done: Arc::new(AtomicBool::new(false)),
            channel: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// Creates a control channel for a restored VM (skips the boot wait).
    pub fn new_restored(connector: GuestConnector, session_secret: [u8; 32]) -> Self {
        Self {
            connector,
            session_secret,
            boot_wait_done: Arc::new(AtomicBool::new(true)),
            channel: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// Sends a one-shot RPC through the multiplex channel and awaits a
    /// single response, bounded by `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if the channel cannot be established, if
    /// the call fails, or if `timeout` elapses before a response arrives.
    async fn multiplex_call(
        &self,
        msg_type: MessageType,
        body: Vec<u8>,
        timeout: Duration,
        context: &'static str,
    ) -> Result<Message> {
        let channel = self.get_or_establish_channel().await?;
        let call = channel.call(msg_type, body);
        match tokio::time::timeout(timeout, call).await {
            Ok(result) => result,
            Err(_) => Err(Error::Guest(format!(
                "multiplex {context} timed out after {timeout:?}"
            ))),
        }
    }

    /// Returns the lazily-established [`MultiplexChannel`], constructing
    /// or reconstructing it if the current one is absent or dead.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if the underlying connect + handshake
    /// fails, or if the peer does not advertise
    /// [`PROTO_FLAG_SUPPORTS_MULTIPLEX`].
    ///
    /// [`PROTO_FLAG_SUPPORTS_MULTIPLEX`]: void_box_protocol::PROTO_FLAG_SUPPORTS_MULTIPLEX
    async fn get_or_establish_channel(&self) -> Result<MultiplexChannel> {
        let mut guard = self.channel.lock().await;

        if let Some(channel) = guard.as_ref() {
            if !channel.is_dead() {
                return Ok(channel.clone());
            }
            debug!("control_channel: multiplex channel dead, reconstructing");
            *guard = None;
        }

        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret;
        let boot_wait_done = Arc::clone(&self.boot_wait_done);

        let channel = tokio::task::spawn_blocking(move || {
            establish_multiplex_channel(
                &connector,
                &session_secret,
                &boot_wait_done,
                HANDSHAKE_READ_TIMEOUT,
                "multiplex-establish",
            )
        })
        .await
        .map_err(|e| Error::Guest(format!("multiplex establish task panicked: {e}")))??;

        *guard = Some(channel.clone());
        Ok(channel)
    }

    /// Eagerly establishes the persistent multiplex channel.
    ///
    /// After [`MicroVm::from_snapshot`] the guest kernel is in HLT/NOHZ-idle
    /// and the guest-agent's accept loop is not yet scheduled. Running this
    /// alongside the vCPU threads drives the vsock accept, Ping/Pong, and
    /// reader-thread startup in parallel with the caller's work, so the
    /// first real RPC finds the multiplex channel already live and its
    /// retry loop already converged.
    ///
    /// Failures are swallowed; the first RPC will re-attempt establishment.
    ///
    /// [`MicroVm::from_snapshot`]: crate::vmm::MicroVm::from_snapshot
    pub async fn warm_handshake(&self) {
        let _ = self.get_or_establish_channel().await;
    }

    /// Sends an exec request and waits for the response.
    ///
    /// Routes through the persistent multiplex channel: allocates a fresh
    /// request_id, submits the request, and drains output chunks until the
    /// terminal `ExecResponse` frame arrives.
    pub async fn send_exec_request(&self, request: &ExecRequest) -> Result<ExecResponse> {
        let body = serde_json::to_vec(request)?;
        let timeout = resolve_exec_read_timeout(request.timeout_secs);
        let channel = self.get_or_establish_channel().await?;
        let mut rx = channel
            .call_stream(
                MessageType::ExecRequest,
                body,
                Terminator::OnMessageType(MessageType::ExecResponse),
            )
            .await?;

        let drain = async {
            while let Some(msg) = rx.recv().await {
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
            Err(Error::Guest(
                "exec stream closed without ExecResponse".into(),
            ))
        };

        apply_exec_timeout(timeout, drain).await
    }

    /// Sends an exec request and streams output chunks as they arrive via callback.
    pub async fn send_exec_request_streaming<F>(
        &self,
        request: &ExecRequest,
        mut on_chunk: F,
    ) -> Result<ExecResponse>
    where
        F: FnMut(ExecOutputChunk) + Send + 'static,
    {
        let body = serde_json::to_vec(request)?;
        let timeout = resolve_exec_read_timeout(request.timeout_secs);
        let channel = self.get_or_establish_channel().await?;
        let mut rx = channel
            .call_stream(
                MessageType::ExecRequest,
                body,
                Terminator::OnMessageType(MessageType::ExecResponse),
            )
            .await?;

        let drain = async {
            while let Some(msg) = rx.recv().await {
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
            Err(Error::Guest(
                "exec streaming channel closed without ExecResponse".into(),
            ))
        };

        apply_exec_timeout(timeout, drain).await
    }

    /// Sends an exec request and streams output chunks via an async mpsc sender.
    pub async fn send_exec_request_streaming_async(
        &self,
        request: &ExecRequest,
        chunk_tx: tokio::sync::mpsc::Sender<ExecOutputChunk>,
    ) -> Result<ExecResponse> {
        let body = serde_json::to_vec(request)?;
        let timeout = resolve_exec_read_timeout(request.timeout_secs);
        let channel = self.get_or_establish_channel().await?;
        let mut rx = channel
            .call_stream(
                MessageType::ExecRequest,
                body,
                Terminator::OnMessageType(MessageType::ExecResponse),
            )
            .await?;

        let drain = async {
            while let Some(msg) = rx.recv().await {
                match msg.msg_type {
                    MessageType::ExecOutputChunk => {
                        match serde_json::from_slice::<ExecOutputChunk>(&msg.payload) {
                            Ok(chunk) => {
                                let _ = chunk_tx.send(chunk).await;
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
            Err(Error::Guest(
                "exec streaming channel closed without ExecResponse".into(),
            ))
        };

        apply_exec_timeout(timeout, drain).await
    }

    /// Writes a file to the guest filesystem using the native WriteFile protocol.
    pub async fn send_write_file(&self, path: &str, content: &[u8]) -> Result<WriteFileResponse> {
        let body = serde_json::to_vec(&WriteFileRequest {
            path: path.to_string(),
            content: content.to_vec(),
            create_parents: true,
        })?;
        let msg = self
            .multiplex_call(
                MessageType::WriteFile,
                body,
                Duration::from_secs(30),
                "WriteFile",
            )
            .await?;
        ensure_response_type(&msg, MessageType::WriteFileResponse, "WriteFile")?;
        Ok(serde_json::from_slice(&msg.payload)?)
    }

    /// Creates directories in the guest filesystem (mkdir -p).
    pub async fn send_mkdir_p(&self, path: &str) -> Result<MkdirPResponse> {
        let body = serde_json::to_vec(&MkdirPRequest {
            path: path.to_string(),
        })?;
        let msg = self
            .multiplex_call(MessageType::MkdirP, body, Duration::from_secs(10), "MkdirP")
            .await?;
        ensure_response_type(&msg, MessageType::MkdirPResponse, "MkdirP")?;
        Ok(serde_json::from_slice(&msg.payload)?)
    }

    /// Checks if a file exists in the guest filesystem.
    pub async fn send_file_stat(&self, path: &str) -> Result<FileStatResponse> {
        let body = serde_json::to_vec(&FileStatRequest {
            path: path.to_string(),
        })?;
        let msg = self
            .multiplex_call(
                MessageType::FileStat,
                body,
                Duration::from_secs(10),
                "FileStat",
            )
            .await?;
        ensure_response_type(&msg, MessageType::FileStatResponse, "FileStat")?;
        Ok(serde_json::from_slice(&msg.payload)?)
    }

    /// Reads a file from the guest filesystem.
    pub async fn send_read_file(&self, path: &str) -> Result<ReadFileResponse> {
        let body = serde_json::to_vec(&ReadFileRequest {
            path: path.to_string(),
        })?;
        let msg = self
            .multiplex_call(
                MessageType::ReadFile,
                body,
                Duration::from_secs(30),
                "ReadFile",
            )
            .await?;
        ensure_response_type(&msg, MessageType::ReadFileResponse, "ReadFile")?;
        Ok(serde_json::from_slice(&msg.payload)?)
    }

    /// Opens a persistent telemetry subscription through the multiplex channel.
    ///
    /// Allocates a request_id for the subscription, sends
    /// `SubscribeTelemetry`, and runs a background task that forwards
    /// every [`TelemetryBatch`] frame to `on_batch` until the channel
    /// dies or the subscription is cancelled by a channel-lifetime end.
    pub async fn subscribe_telemetry<F>(
        &self,
        opts: &TelemetrySubscribeRequest,
        mut on_batch: F,
    ) -> Result<()>
    where
        F: FnMut(TelemetryBatch) + Send + 'static,
    {
        let body = serde_json::to_vec(opts).unwrap_or_default();
        let interval_ms = opts.interval_ms;
        let channel = self.get_or_establish_channel().await?;
        let mut rx = channel
            .call_stream(
                MessageType::SubscribeTelemetry,
                body,
                Terminator::ChannelLifetime,
            )
            .await?;

        info!("Telemetry subscription active (interval={}ms)", interval_ms);

        while let Some(msg) = rx.recv().await {
            if msg.msg_type != MessageType::TelemetryData {
                warn!(
                    "Unexpected message type in telemetry stream: {:?}",
                    msg.msg_type
                );
                continue;
            }
            match serde_json::from_slice::<TelemetryBatch>(&msg.payload) {
                Ok(batch) => on_batch(batch),
                Err(e) => warn!("Failed to parse TelemetryBatch: {}", e),
            }
        }

        info!("Telemetry subscription ended");
        Ok(())
    }

    /// Waits for the guest to signal snapshot readiness.
    ///
    /// Sends a `SnapshotReady` message through the multiplex channel and
    /// waits for the guest to echo it back.
    pub async fn wait_for_snapshot_ready(&self, timeout: Duration) -> Result<()> {
        let msg = self
            .multiplex_call(
                MessageType::SnapshotReady,
                Vec::new(),
                timeout,
                "SnapshotReady",
            )
            .await?;
        ensure_response_type(&msg, MessageType::SnapshotReady, "SnapshotReady")?;
        debug!("control_channel: guest confirmed SnapshotReady");
        Ok(())
    }

    /// Opens a PTY session on the guest, returning a [`super::pty_session::PtySession`] that owns the connection.
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
    // Mark the first attempt for logging / future diagnostics. We used to
    // block here on a fixed `sleep(4s)` as a worst-case "wait for guest
    // kernel boot" pad; profiling showed that single sleep was ~85% of
    // cold-boot wall-clock. Polling connect() on a short interval below
    // reaches the guest-agent as soon as it binds the vsock port — the
    // guest is typically ready in 200-800ms.
    let first_attempt = boot_wait_done
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();

    // Cold boot: give the guest kernel a head start before we hammer
    // the vsock. Host-side AF_VSOCK connect goes through the kernel
    // vhost-vsock driver, which RSTs or corners itself when the guest's
    // virtio-vsock device isn't fully initialized. The userspace
    // backend buffers connects behind its worker thread and doesn't
    // suffer, but we take the common path for both.
    if first_attempt {
        std::thread::sleep(BOOT_WAIT);
    }

    // Initial delay sized for typical guest boot probe cadence (~25ms).
    // Max cap kept small so a late-booting guest costs at most ~250ms of
    // over-sleep, not 2s.
    let mut delay = Duration::from_millis(25);
    let max_delay = Duration::from_millis(250);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut attempt: u32 = 0;
    let t_start = Instant::now();
    let mut attempt_timeout = handshake_timeout;

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
                delay = std::cmp::min(delay * 2, max_delay);
                continue;
            }
        };

        // Handshake: Ping -> Pong
        if let Err(e) = s.set_read_timeout(Some(attempt_timeout)) {
            debug!(
                "control_channel[{context}]: attempt {} set_read_timeout failed: {}",
                attempt, e
            );
            std::thread::sleep(delay);
            delay = std::cmp::min(delay * 2, max_delay);
            continue;
        }

        // Build Ping payload via protocol helper — advertises this host's
        // feature flags (multiplex capability).
        let ping_msg = Message {
            msg_type: MessageType::Ping,
            payload: void_box_protocol::build_ping_payload(
                session_secret,
                void_box_protocol::PROTO_FLAG_SUPPORTS_MULTIPLEX,
            ),
        };
        if s.write_all(&ping_msg.serialize()).is_err() {
            debug!(
                "control_channel[{context}]: attempt {} failed to send Ping",
                attempt
            );
            std::thread::sleep(delay);
            delay = std::cmp::min(delay * 2, max_delay);
            continue;
        }
        match Message::read_from_sync(&mut *s) {
            Ok(msg) if msg.msg_type == MessageType::Pong => {
                let (peer_version, peer_flags) =
                    void_box_protocol::parse_pong_payload(&msg.payload);
                let peer_supports_multiplex =
                    peer_flags & void_box_protocol::PROTO_FLAG_SUPPORTS_MULTIPLEX != 0;
                debug!(
                    "control_channel[{context}]: handshake OK \
                     (peer_version={}, peer_flags={:#x}, peer_multiplex={}, \
                      cold={}, attempts={}, elapsed={:?})",
                    peer_version,
                    peer_flags,
                    peer_supports_multiplex,
                    first_attempt,
                    attempt,
                    t_start.elapsed(),
                );
                return Ok(s);
            }
            Ok(msg) => {
                debug!(
                    "control_channel[{context}]: attempt {} unexpected handshake message: {:?}",
                    attempt, msg.msg_type
                );
                std::thread::sleep(delay);
                delay = std::cmp::min(delay * 2, max_delay);
            }
            Err(e) => {
                debug!(
                    "control_channel[{context}]: attempt {} handshake read failed: {} \
                     (timeout={:?})",
                    attempt, e, attempt_timeout
                );
                std::thread::sleep(delay);
                delay = std::cmp::min(delay * 2, max_delay);
                attempt_timeout = std::cmp::min(attempt_timeout * 2, MAX_HANDSHAKE_READ_TIMEOUT);
            }
        }
    }
}

/// Connects, handshakes, verifies multiplex support, and returns a ready
/// [`MultiplexChannel`].
///
/// The returned channel owns one dedicated reader thread demultiplexing
/// incoming frames by request_id. The writer half is a Mutex-guarded
/// [`Box<dyn GuestStream>`] shared across concurrent RPC callers.
///
/// # Errors
///
/// Returns [`Error::Guest`] if the connect or handshake retry loop
/// exhausts its deadline, if the peer advertises an older protocol
/// that does not support multiplex, or if the `dup(2)` syscall used to
/// split read/write halves fails.
pub(crate) fn establish_multiplex_channel(
    connector: &GuestConnector,
    session_secret: &[u8; 32],
    boot_wait_done: &AtomicBool,
    handshake_timeout: Duration,
    context: &str,
) -> Result<MultiplexChannel> {
    let stream = connect_with_handshake_sync(
        connector,
        session_secret,
        boot_wait_done,
        handshake_timeout,
        context,
    )?;
    upgrade_stream_to_multiplex(stream, context)
}

/// Upgrades an already-handshaken [`GuestStream`] into a [`MultiplexChannel`].
///
/// Duplicates the file descriptor so the reader thread and the shared
/// writer each own a distinct fd backed by the same kernel socket.
fn upgrade_stream_to_multiplex(
    writer_stream: Box<dyn GuestStream>,
    context: &str,
) -> Result<MultiplexChannel> {
    let reader_stream = writer_stream.try_clone_box().map_err(|e| {
        Error::Guest(format!(
            "control_channel[{context}]: failed to dup stream fd for reader: {e}"
        ))
    })?;

    reader_stream.set_read_timeout(None).map_err(|e| {
        Error::Guest(format!(
            "control_channel[{context}]: failed to clear read timeout on reader fd: {e}"
        ))
    })?;

    let reader: Box<dyn Read + Send> = Box::new(GuestStreamReader {
        inner: reader_stream,
    });
    let sender: Arc<dyn FrameSender> = Arc::new(StreamFrameSender {
        stream: StdMutex::new(writer_stream),
    });

    Ok(MultiplexChannel::new(reader, sender))
}

/// Adapts a [`Box<dyn GuestStream>`] into [`Box<dyn Read + Send>`] for the
/// multiplex reader thread.
struct GuestStreamReader {
    inner: Box<dyn GuestStream>,
}

impl Read for GuestStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

/// Production [`FrameSender`] wrapping a [`GuestStream`] writer half.
///
/// Holds a [`std::sync::Mutex`] over the boxed stream so concurrent
/// RPC callers never interleave their frame bytes on the wire. The
/// mutex is only held for the duration of one `write_all`, which is
/// bounded by the size of the frame — telemetry and RPC payloads are
/// typically < 64 KiB, so contention is minimal.
struct StreamFrameSender {
    stream: StdMutex<Box<dyn GuestStream>>,
}

impl FrameSender for StreamFrameSender {
    fn send(&self, frame: &[u8]) -> Result<()> {
        let mut guard = self
            .stream
            .lock()
            .map_err(|_| Error::Guest("multiplex sender stream poisoned".into()))?;
        guard
            .write_all(frame)
            .map_err(|e| Error::Guest(format!("frame send failed: {e}")))?;
        Ok(())
    }
}

/// Applies an optional deadline to an async drain future.
///
/// Matches the previous blocking-stream semantics: `None` means wait
/// forever (service mode / long-running LLM exec); `Some(d)` bounds the
/// wall-clock wait and surfaces a clear error on expiry.
async fn apply_exec_timeout<Fut>(timeout: Option<Duration>, fut: Fut) -> Result<ExecResponse>
where
    Fut: std::future::Future<Output = Result<ExecResponse>>,
{
    match timeout {
        Some(deadline) => match tokio::time::timeout(deadline, fut).await {
            Ok(result) => result,
            Err(_) => Err(Error::Guest(format!("exec timed out after {deadline:?}"))),
        },
        None => fut.await,
    }
}

/// Verifies that a multiplex response matches the expected [`MessageType`].
///
/// # Errors
///
/// Returns [`Error::Guest`] if `msg.msg_type != expected`.
fn ensure_response_type(msg: &Message, expected: MessageType, context: &'static str) -> Result<()> {
    if msg.msg_type == expected {
        return Ok(());
    }
    Err(Error::Guest(format!(
        "Unexpected response type for {context}: {:?}",
        msg.msg_type
    )))
}
