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
use void_box_protocol::SessionSecret;

use crate::backend::multiplex::{FrameSender, MultiplexChannel, SendError, Terminator};
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

/// Upper bound for the exponential per-attempt handshake read timeout.
///
/// 150 ms is the ceiling validated against agent workloads — long
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

/// Per-blocking-wait bound on multiplex frame writes to the guest.
///
/// Applied as the send timeout (`SO_SNDTIMEO`) on the post-handshake
/// writer half. The bound covers each blocking wait inside `write_all`,
/// not the whole frame: a slow-but-draining guest resets it with every
/// accepted chunk, so only a guest that stops draining the socket
/// entirely trips it. A timed-out write may leave a truncated frame on
/// the wire, so the multiplex layer marks the channel dead and the next
/// RPC reconnects.
const MULTIPLEX_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval at which the multiplex reader re-checks the channel's
/// shutdown flag while blocked on a read.
///
/// Applied as `SO_RCVTIMEO` on the post-handshake reader half.
/// [`GuestStreamReader`] swallows each timeout and retries, so the
/// framing layer above never observes it; the interval only bounds how
/// long reader-thread exit lags a shutdown or channel-death signal.
const READER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Slack added to [`connect_deadline`] when bounding channel
/// establishment at the async layer.
///
/// Establishment awaits the channel mutex and a `spawn_blocking` join,
/// neither of which the inner connect/handshake deadline covers: a
/// caller can spend up to a full deadline queued behind a concurrent
/// establish before its own attempt starts. The margin gives the inner
/// loop its full window in that worst case.
const ESTABLISH_TIMEOUT_MARGIN: Duration = Duration::from_secs(30);

/// Deadline for the connect/handshake loop against a booting guest.
///
/// The 30 s default covers production-size initramfs boots on bare-metal
/// hosts (see the AGENTS.md known-issues entry on boot timeouts). Slow
/// validation environments — nested virtualization in particular — can
/// extend it with `VOID_BOX_CONNECT_DEADLINE_SECS`; the override is opt-in
/// and is clamped to [default, 1 h]: it can only lengthen the deadline
/// (default behavior is unchanged wherever the variable is unset), and the
/// upper bound keeps the `Instant + Duration` deadline arithmetic
/// panic-free if the variable holds an absurd value.
fn connect_deadline() -> Duration {
    const DEFAULT_CONNECT_DEADLINE_SECS: u64 = 30;
    const MAX_CONNECT_DEADLINE_SECS: u64 = 3600;
    let secs = std::env::var("VOID_BOX_CONNECT_DEADLINE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(DEFAULT_CONNECT_DEADLINE_SECS, |value| {
            value.clamp(DEFAULT_CONNECT_DEADLINE_SECS, MAX_CONNECT_DEADLINE_SECS)
        });
    Duration::from_secs(secs)
}

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

    /// Sets the write timeout, bounding each blocking wait inside a
    /// write. `None` means blocking (no timeout).
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;

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
    session_secret: SessionSecret,
    /// Whether the initial boot wait has been applied.
    boot_wait_done: Arc<AtomicBool>,
    /// Cold-boot wait applied once before the first connect attempt.
    ///
    /// Set to a non-zero value for the kernel vhost-vsock backend,
    /// whose `libc::connect` corners the driver when fired before the
    /// guest's virtio-vsock device is up. The userspace vsock backend
    /// buffers connects behind its worker and uses [`Duration::ZERO`].
    boot_wait: Duration,
    /// Serializes establishment attempts so concurrent RPCs on a dead
    /// channel produce one reconnect instead of a stampede.
    establish_lock: Arc<AsyncMutex<()>>,
    /// Lazily-established multiplex channel. Re-established on death.
    ///
    /// Sync mutex, locked only to clone or replace the handle, so
    /// [`shutdown`](Self::shutdown) never waits on `establish_lock`
    /// (held across a full connect deadline by a wedged establishment).
    channel: Arc<StdMutex<Option<MultiplexChannel>>>,
    /// Set by [`shutdown`](Self::shutdown); establishment refuses
    /// once set.
    shutting_down: Arc<AtomicBool>,
}

impl ControlChannel {
    /// Creates a control channel that skips the cold-boot wait.
    ///
    /// Equivalent to [`Self::with_boot_wait`] with `boot_wait =
    /// Duration::ZERO`. Appropriate for userspace-vsock connectors
    /// where connect is buffered against guest readiness.
    pub fn new(connector: GuestConnector, session_secret: SessionSecret) -> Self {
        Self::with_boot_wait(connector, session_secret, Duration::ZERO)
    }

    /// Creates a control channel that pads the first connect attempt
    /// with `boot_wait`.
    ///
    /// Intended for connectors backed by the kernel vhost-vsock
    /// driver so the guest's virtio-vsock device has time to come up
    /// before the host's first `libc::connect` reaches the driver.
    pub fn with_boot_wait(
        connector: GuestConnector,
        session_secret: SessionSecret,
        boot_wait: Duration,
    ) -> Self {
        Self {
            connector,
            session_secret,
            boot_wait_done: Arc::new(AtomicBool::new(false)),
            boot_wait,
            establish_lock: Arc::new(AsyncMutex::new(())),
            channel: Arc::new(StdMutex::new(None)),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Creates a control channel for a restored VM (skips the boot wait).
    pub fn new_restored(connector: GuestConnector, session_secret: SessionSecret) -> Self {
        Self {
            connector,
            session_secret,
            boot_wait_done: Arc::new(AtomicBool::new(true)),
            boot_wait: Duration::ZERO,
            establish_lock: Arc::new(AsyncMutex::new(())),
            channel: Arc::new(StdMutex::new(None)),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Shuts the channel down for VM teardown: fails every pending RPC
    /// (including those with no timeout of their own, e.g. service-mode
    /// execs), stops the reader thread, aborts an in-flight connect
    /// loop, and makes establishment refuse. Idempotent.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let channel = self.lock_channel_slot().take();
        if let Some(channel) = channel {
            channel.shutdown();
        }
    }

    fn lock_channel_slot(&self) -> std::sync::MutexGuard<'_, Option<MultiplexChannel>> {
        // Held only to clone or replace the handle; poison recovery is
        // safe and keeps shutdown working after a panicked RPC task.
        self.channel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    /// Bounded end-to-end: this await runs *before* every per-RPC
    /// timeout wrapper, so without its own deadline a wedged
    /// establishment (a mutex holder that never returns, a blocking
    /// task that never joins) would hang the RPC with no timer armed
    /// anywhere.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if the underlying connect + handshake
    /// fails, if the peer does not advertise
    /// [`PROTO_FLAG_SUPPORTS_MULTIPLEX`], or if establishment exceeds
    /// the connect deadline plus [`ESTABLISH_TIMEOUT_MARGIN`].
    ///
    /// [`PROTO_FLAG_SUPPORTS_MULTIPLEX`]: void_box_protocol::PROTO_FLAG_SUPPORTS_MULTIPLEX
    async fn get_or_establish_channel(&self) -> Result<MultiplexChannel> {
        let deadline = connect_deadline() + ESTABLISH_TIMEOUT_MARGIN;
        match tokio::time::timeout(deadline, self.get_or_establish_channel_unbounded()).await {
            Ok(result) => result,
            Err(_) => Err(Error::Guest(format!(
                "multiplex channel establishment timed out after {deadline:?} \
                 (includes time queued behind concurrent establish attempts)"
            ))),
        }
    }

    async fn get_or_establish_channel_unbounded(&self) -> Result<MultiplexChannel> {
        let _establish_guard = self.establish_lock.lock().await;

        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::Guest("control channel is shut down".into()));
        }

        {
            let mut slot = self.lock_channel_slot();
            if let Some(channel) = slot.as_ref() {
                if !channel.is_dead() {
                    return Ok(channel.clone());
                }
                debug!("control_channel: multiplex channel dead, reconstructing");
                *slot = None;
            }
        }

        let connector = Arc::clone(&self.connector);
        let session_secret = self.session_secret.clone();
        let boot_wait_done = Arc::clone(&self.boot_wait_done);
        let boot_wait = self.boot_wait;
        let shutting_down = Arc::clone(&self.shutting_down);

        let channel = tokio::task::spawn_blocking(move || {
            establish_multiplex_channel(
                &connector,
                &session_secret,
                &boot_wait_done,
                boot_wait,
                HANDSHAKE_READ_TIMEOUT,
                &shutting_down,
                "multiplex-establish",
            )
        })
        .await
        .map_err(|e| Error::Guest(format!("multiplex establish task panicked: {e}")))??;

        // Re-check under the slot lock: a shutdown that raced this
        // establishment must not be handed a fresh live channel.
        let mut slot = self.lock_channel_slot();
        if self.shutting_down.load(Ordering::SeqCst) {
            channel.shutdown();
            return Err(Error::Guest("control channel is shut down".into()));
        }
        *slot = Some(channel.clone());
        Ok(channel)
    }

    /// Eagerly establishes the persistent multiplex channel.
    ///
    /// After `MicroVm::from_snapshot` the guest kernel is in HLT/NOHZ-idle
    /// and the guest-agent's accept loop is not yet scheduled. Running this
    /// alongside the vCPU threads drives the vsock accept, Ping/Pong, and
    /// reader-thread startup in parallel with the caller's work, so the
    /// first real RPC finds the multiplex channel already live and its
    /// retry loop already converged.
    ///
    /// Failures are swallowed; the first RPC will re-attempt establishment.
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
        let session_secret = self.session_secret.clone();
        let boot_wait_done = Arc::clone(&self.boot_wait_done);
        let shutting_down = Arc::clone(&self.shutting_down);
        tokio::task::spawn_blocking(move || {
            super::pty_session::PtySession::open(
                &connector,
                &session_secret,
                &boot_wait_done,
                &shutting_down,
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
///
/// `abort` is polled between attempts: a channel shutdown ends the
/// retry loop within one backoff delay instead of running out the full
/// connect deadline.
pub(crate) fn connect_with_handshake_sync(
    connector: &GuestConnector,
    session_secret: &SessionSecret,
    boot_wait_done: &AtomicBool,
    boot_wait: Duration,
    handshake_timeout: Duration,
    abort: &AtomicBool,
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

    // Callers who know their connector cannot tolerate rapid retries
    // against a still-booting guest (currently the kernel vhost-vsock
    // backend) pass a non-zero `boot_wait` to pad the first attempt.
    // Userspace backends pass `Duration::ZERO` and skip this entirely.
    if first_attempt && boot_wait > Duration::ZERO {
        std::thread::sleep(boot_wait);
    }

    // Initial delay sized for typical guest boot probe cadence (~25ms).
    // Max cap kept small so a late-booting guest costs at most ~250ms of
    // over-sleep, not 2s.
    let mut delay = Duration::from_millis(25);
    let max_delay = Duration::from_millis(250);
    let deadline = Instant::now() + connect_deadline();
    let mut attempt: u32 = 0;
    let t_start = Instant::now();
    let mut attempt_timeout = handshake_timeout;

    loop {
        if abort.load(Ordering::SeqCst) {
            return Err(Error::Guest(
                "control_channel: connect aborted (channel shut down)".into(),
            ));
        }

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
                session_secret.expose_secret(),
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
    session_secret: &SessionSecret,
    boot_wait_done: &AtomicBool,
    boot_wait: Duration,
    handshake_timeout: Duration,
    abort: &AtomicBool,
    context: &str,
) -> Result<MultiplexChannel> {
    let stream = connect_with_handshake_sync(
        connector,
        session_secret,
        boot_wait_done,
        boot_wait,
        handshake_timeout,
        abort,
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

    // The dup'd fds share one underlying socket, so both timeouts apply
    // to it: reads use the receive timeout, writes the send timeout.
    // This also replaces the handshake read timeout left on the socket.
    reader_stream
        .set_read_timeout(Some(READER_SHUTDOWN_POLL_INTERVAL))
        .map_err(|e| {
            Error::Guest(format!(
                "control_channel[{context}]: failed to set read timeout on reader fd: {e}"
            ))
        })?;
    writer_stream
        .set_write_timeout(Some(MULTIPLEX_SEND_TIMEOUT))
        .map_err(|e| {
            Error::Guest(format!(
                "control_channel[{context}]: failed to set send timeout on writer fd: {e}"
            ))
        })?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let reader: Box<dyn Read + Send> = Box::new(GuestStreamReader {
        inner: reader_stream,
        shutdown: Arc::clone(&shutdown),
    });
    let sender: Arc<dyn FrameSender> = Arc::new(StreamFrameSender {
        stream: StdMutex::new(writer_stream),
        shutdown: Arc::clone(&shutdown),
    });

    Ok(MultiplexChannel::new(reader, sender, shutdown))
}

/// Adapts a [`Box<dyn GuestStream>`] into [`Box<dyn Read + Send>`] for the
/// multiplex reader thread, giving the blocked reader a shutdown check.
///
/// The stream carries a bounded read timeout
/// ([`READER_SHUTDOWN_POLL_INTERVAL`]); without one, a reader blocked on
/// a peer that stalls without closing its fd is unreachable — channel
/// death would be detectable only via EOF. The timeout must not escape
/// this adapter: the framing layer above (`Message::read_from_sync`)
/// treats any error as fatal, and a timeout surfacing between a frame's
/// header and payload would kill a healthy channel that is merely idle.
/// So timeouts are swallowed and the read retried, and when the shutdown
/// flag is set the read reports end-of-stream, which drives the reader
/// loop through its normal fail-pending-slots exit path.
struct GuestStreamReader {
    inner: Box<dyn GuestStream>,
    shutdown: Arc<AtomicBool>,
}

impl Read for GuestStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return Ok(0);
            }
            match self.inner.read(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    // On a conforming blocking socket the receive
                    // timeout paces this loop at one iteration per
                    // interval; if the fd were ever non-blocking,
                    // `WouldBlock` would return instantly and the loop
                    // would spin a core. The sleep caps that at ~100
                    // iterations/s and is invisible next to the 1 s
                    // tick on the normal path.
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                result => return result,
            }
        }
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
    /// Checked under the stream mutex before writing. A failed send may
    /// leave a truncated frame on the wire; a sender already queued on
    /// the mutex at that moment would otherwise append a well-formed
    /// frame right after the truncated bytes, handing the guest's
    /// framing layer garbage that can alias as a valid message. The
    /// mutex-held check closes that window: once the channel is marked
    /// dead, queued senders bail before touching the stream.
    shutdown: Arc<AtomicBool>,
}

impl FrameSender for StreamFrameSender {
    fn send(&self, frame: &[u8]) -> std::result::Result<(), SendError> {
        let mut guard = self.stream.lock().map_err(|_| {
            SendError::NothingSent(Error::Guest("multiplex sender stream poisoned".into()))
        })?;
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(SendError::NothingSent(Error::Guest(
                "frame send failed: channel is shut down".into(),
            )));
        }
        // Hand-rolled write loop instead of `write_all`: on failure the
        // channel's fate depends on whether any byte of this frame
        // reached the wire, and `write_all` discards that. A send
        // timeout with zero progress (e.g. the socket buffer was
        // already full under transient starvation) fails only this RPC;
        // a mid-frame failure poisons the wire and kills the channel.
        let mut written = 0usize;
        while written < frame.len() {
            match guard.write(&frame[written..]) {
                Ok(0) => {
                    let error = Error::Guest(format!(
                        "frame send failed after {written}/{} bytes: stream accepted no bytes",
                        frame.len()
                    ));
                    return Err(classify_send_error(written, error));
                }
                Ok(bytes_written) => written += bytes_written,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let error = Error::Guest(format!(
                        "frame send failed after {written}/{} bytes: {e}",
                        frame.len()
                    ));
                    return Err(classify_send_error(written, error));
                }
            }
        }
        Ok(())
    }
}

fn classify_send_error(bytes_written: usize, error: Error) -> SendError {
    if bytes_written == 0 {
        SendError::NothingSent(error)
    } else {
        SendError::Truncated(error)
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::net::UnixStream;

    use crate::backend::multiplex::{build_frame, decode_payload};

    /// [`GuestStream`] over a Unix socket pair, standing in for the
    /// vsock transports — same fd semantics, no VM required.
    struct UnixGuestStream {
        stream: UnixStream,
    }

    impl Read for UnixGuestStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.stream.read(buf)
        }
    }

    impl Write for UnixGuestStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.stream.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    impl GuestStream for UnixGuestStream {
        fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.stream.set_read_timeout(timeout)
        }

        fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.stream.set_write_timeout(timeout)
        }

        fn as_raw_fd(&self) -> RawFd {
            std::os::unix::io::AsRawFd::as_raw_fd(&self.stream)
        }

        fn try_clone_box(&self) -> io::Result<Box<dyn GuestStream>> {
            Ok(Box::new(UnixGuestStream {
                stream: self.stream.try_clone()?,
            }))
        }
    }

    /// The reader's bounded read timeout must never surface mid-frame: a
    /// guest that pauses longer than the poll interval between a frame's
    /// header bytes is idle, not dead, and the response must still be
    /// delivered intact.
    #[tokio::test]
    async fn multiplex_reader_preserves_framing_across_timeout_ticks() {
        let (host_side, mut guest_side) = UnixStream::pair().expect("socket pair");
        let host_stream: Box<dyn GuestStream> = Box::new(UnixGuestStream { stream: host_side });
        let channel = upgrade_stream_to_multiplex(host_stream, "test").expect("upgrade");

        let guest = std::thread::spawn(move || {
            let request = Message::read_from_sync(&mut guest_side).expect("read request");
            let (request_id, _body) = decode_payload(&request.payload).expect("payload");
            let frame = build_frame(MessageType::Pong, request_id, b"ack");
            // Split the response mid-header, pausing past the reader's
            // poll interval so a timeout fires with a frame in flight.
            guest_side.write_all(&frame[..3]).expect("write head");
            std::thread::sleep(READER_SHUTDOWN_POLL_INTERVAL + Duration::from_millis(300));
            guest_side.write_all(&frame[3..]).expect("write tail");
        });

        // Derived from the poll interval so raising the production
        // constant cannot silently turn this assertion into a timeout.
        let outer_deadline = READER_SHUTDOWN_POLL_INTERVAL + Duration::from_secs(4);
        let response = tokio::time::timeout(
            outer_deadline,
            channel.call(MessageType::Ping, b"hello".to_vec()),
        )
        .await
        .expect("call must not hang")
        .expect("split frame must still parse");
        assert_eq!(response.msg_type, MessageType::Pong);
        assert_eq!(response.payload, b"ack");
        guest.join().expect("guest thread");
    }

    /// `shutdown()` must unblock a pending RPC whose guest went silent
    /// without closing its fd — the case EOF-based death detection
    /// cannot see.
    #[tokio::test]
    async fn multiplex_shutdown_fails_pending_rpc_on_silent_guest() {
        let (host_side, guest_side) = UnixStream::pair().expect("socket pair");
        let host_stream: Box<dyn GuestStream> = Box::new(UnixGuestStream { stream: host_side });
        let channel = upgrade_stream_to_multiplex(host_stream, "test").expect("upgrade");

        let pending_channel = channel.clone();
        let pending = tokio::spawn(async move {
            pending_channel
                .call(MessageType::Ping, b"hello".to_vec())
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        channel.shutdown();

        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("pending call must fail promptly after shutdown")
            .expect("task must not panic");
        assert!(result.is_err(), "pending call must error after shutdown");
        // guest_side stayed open throughout: the channel died without EOF.
        drop(guest_side);
    }

    /// `shutdown` must fail a pending RPC promptly even when the guest
    /// went silent without closing its fd, and must refuse to
    /// re-establish afterward.
    #[tokio::test]
    async fn control_channel_shutdown_fails_pending_and_refuses_reestablish() {
        let (host_side, guest_side) = UnixStream::pair().expect("socket pair");
        let host_stream: Box<dyn GuestStream> = Box::new(UnixGuestStream { stream: host_side });
        let channel = upgrade_stream_to_multiplex(host_stream, "test").expect("upgrade");

        let connector: GuestConnector =
            Arc::new(|| Err(Error::Guest("no guest to connect to".into())));
        let control = ControlChannel::new(connector, SessionSecret::new([0u8; 32]));
        *control.lock_channel_slot() = Some(channel.clone());

        let pending_channel = channel.clone();
        let pending = tokio::spawn(async move {
            pending_channel
                .call(MessageType::Ping, b"hello".to_vec())
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        control.shutdown();

        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("pending call must fail promptly after shutdown")
            .expect("task must not panic");
        assert!(result.is_err(), "pending call must error after shutdown");

        let establish_err = match control.get_or_establish_channel().await {
            Ok(_) => panic!("establishment after shutdown must be refused"),
            Err(e) => e,
        };
        assert!(
            establish_err.to_string().contains("shut down"),
            "unexpected error: {establish_err}"
        );
        // guest_side stayed open throughout: no EOF was involved.
        drop(guest_side);
    }

    /// Once the channel is marked dead, a sender that was queued on the
    /// stream mutex must not write: the wire may hold a truncated frame,
    /// and appending a well-formed frame after it would desynchronize
    /// the guest's framing.
    #[test]
    fn frame_sender_refuses_to_write_after_shutdown() {
        let (host_side, mut guest_side) = UnixStream::pair().expect("socket pair");
        let shutdown = Arc::new(AtomicBool::new(true));
        let sender = StreamFrameSender {
            stream: StdMutex::new(Box::new(UnixGuestStream { stream: host_side })),
            shutdown: Arc::clone(&shutdown),
        };

        let err = sender
            .send(b"frame bytes")
            .expect_err("send after shutdown must fail");
        assert!(
            matches!(err, SendError::NothingSent(Error::Guest(ref msg)) if msg.contains("shut down")),
            "expected a clean NothingSent failure, got {err:?}"
        );

        guest_side
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set read timeout");
        let mut probe = [0u8; 1];
        let read_result = guest_side.read(&mut probe);
        assert!(
            read_result.is_err(),
            "no bytes may reach the wire after shutdown"
        );
    }

    /// Env mutation is process-global; this is the only test touching the
    /// variable, and it restores the unset state before returning.
    #[test]
    fn connect_deadline_env_override_extends_only() {
        const VAR: &str = "VOID_BOX_CONNECT_DEADLINE_SECS";

        std::env::remove_var(VAR);
        assert_eq!(connect_deadline(), Duration::from_secs(30));

        std::env::set_var(VAR, "240");
        assert_eq!(connect_deadline(), Duration::from_secs(240));

        // The override can only extend the deadline, never shorten it.
        std::env::set_var(VAR, "5");
        assert_eq!(connect_deadline(), Duration::from_secs(30));

        // Garbage falls back to the default.
        std::env::set_var(VAR, "not-a-number");
        assert_eq!(connect_deadline(), Duration::from_secs(30));

        // Absurd values are clamped so the Instant + Duration deadline
        // arithmetic cannot panic on overflow.
        std::env::set_var(VAR, u64::MAX.to_string());
        assert_eq!(connect_deadline(), Duration::from_secs(3600));

        std::env::remove_var(VAR);
    }
}
