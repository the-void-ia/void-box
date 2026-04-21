//! Long-lived multiplexed control channel for host↔guest RPCs.
//!
//! Lever 7b of the startup plan. Replaces the per-RPC
//! `connect_with_handshake_sync` pattern with one persistent connection
//! per sandbox over which every request, response, and stream is
//! multiplexed by a 4-byte `request_id` prefix on each payload.
//!
//! ## Wire Format
//!
//! After the Ping/Pong handshake negotiates
//! [`PROTO_FLAG_SUPPORTS_MULTIPLEX`] on both sides, every subsequent
//! frame's `payload` is logically `[request_id: 4 B LE][body ...]`.
//! The physical [`Message`] frame layout is unchanged; the request_id
//! is an in-payload prefix. [`build_frame`] and [`decode_payload`]
//! centralize the layout so callers never hand-roll offsets.
//!
//! ## Dispatch Model
//!
//! Each caller that issues an RPC:
//!
//! 1. Allocates a fresh `request_id` via an [`AtomicU32`] counter.
//! 2. Registers a dispatch slot (oneshot or stream) keyed on that id.
//! 3. Writes the framed request through the shared [`FrameSender`].
//! 4. Awaits the matching response on its channel.
//!
//! A single dedicated reader thread owns the read half of the stream,
//! demultiplexes each incoming frame by its request_id, and delivers
//! the message to the registered dispatch slot. When the reader
//! detects end-of-stream or a fatal protocol error, it marks the
//! channel dead, fails every still-pending slot, and exits — the next
//! RPC attempt surfaces the error and the caller can open a fresh
//! channel.
//!
//! [`PROTO_FLAG_SUPPORTS_MULTIPLEX`]: void_box_protocol::PROTO_FLAG_SUPPORTS_MULTIPLEX

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use void_box_protocol::{Message, MessageType, ProtocolError};

use crate::{Error, Result};

/// Size of the in-payload request_id prefix (little-endian u32).
pub const REQUEST_ID_PREFIX: usize = 4;

/// Buffer size for streaming dispatch mpsc channels.
///
/// Each exec streams `ExecOutputChunk` frames at up to a few hundred
/// per second; 128 absorbs short bursts without blocking the reader.
const STREAM_BUFFER: usize = 128;

/// How a response for a given request_id is delivered to its caller.
///
/// `Oneshot` completes once with the first matching frame.
/// `Stream` forwards every matching frame until its [`Terminator`] says
/// to close or the channel dies. The terminator lets call sites encode
/// per-operation termination rules: `ExecRequest` terminates on
/// `ExecResponse`; `SubscribeTelemetry` never terminates on its own
/// (only on channel death).
enum Dispatch {
    Oneshot(oneshot::Sender<Message>),
    Stream {
        sender: mpsc::Sender<Message>,
        terminator: Terminator,
    },
}

/// Classifies whether an incoming streaming frame is the terminal one.
#[derive(Debug, Clone, Copy)]
pub enum Terminator {
    /// Stream closes when the reader sees a frame of this type, and the
    /// terminal frame is delivered to the receiver before close.
    OnMessageType(MessageType),
    /// Stream runs until the channel dies (e.g., telemetry).
    ChannelLifetime,
}

/// Writes framed messages to the shared guest connection.
///
/// Implementations must be thread-safe and must ensure that bytes from
/// distinct [`send`](Self::send) calls never interleave on the wire.
/// In production the implementation holds a [`Mutex`] over the write
/// half of the stream; tests use an in-memory buffer.
pub trait FrameSender: Send + Sync {
    /// Writes a fully serialized [`Message`] frame to the guest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if the underlying stream write fails
    /// or if the channel has been marked dead.
    fn send(&self, frame: &[u8]) -> Result<()>;
}

/// Handle to the persistent multiplexed control channel.
///
/// Clones share the same underlying reader thread and pending-slot
/// table. Dropping the final handle signals the reader thread to
/// exit on its next cycle.
#[derive(Clone)]
pub struct MultiplexChannel {
    inner: Arc<Inner>,
}

struct Inner {
    writer: Arc<dyn FrameSender>,
    pending: Arc<Mutex<PendingTable>>,
    next_id: AtomicU32,
}

struct PendingTable {
    slots: HashMap<u32, Dispatch>,
    dead: Option<String>,
}

impl PendingTable {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            dead: None,
        }
    }
}

impl MultiplexChannel {
    /// Constructs a channel around an already-handshaken stream.
    ///
    /// Spawns a dedicated OS thread that owns `reader` and demultiplexes
    /// incoming frames. The reader thread exits — and every still-pending
    /// dispatch fails — when the reader hits EOF, a protocol error, or
    /// sees the writer's [`FrameSender`] dropped.
    ///
    /// # Errors
    ///
    /// Construction itself cannot fail; errors surface on the first
    /// [`call`](Self::call) or [`call_stream`](Self::call_stream) once
    /// the reader thread marks the channel dead.
    pub fn new(reader: Box<dyn Read + Send>, writer: Arc<dyn FrameSender>) -> Self {
        let pending = Arc::new(Mutex::new(PendingTable::new()));
        let inner = Arc::new(Inner {
            writer,
            pending: Arc::clone(&pending),
            next_id: AtomicU32::new(1),
        });

        let reader_pending = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("multiplex-reader".into())
            .spawn(move || reader_loop(reader, reader_pending))
            .expect("spawn multiplex reader");

        Self { inner }
    }

    /// Sends a one-shot RPC and awaits the matching response.
    ///
    /// Allocates a fresh `request_id`, prepends it to `body`, writes the
    /// framed message through the channel's writer, and suspends until
    /// the reader thread delivers a matching response frame (or marks
    /// the channel dead).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if the channel is dead, if sending the
    /// frame fails, or if the reader shuts down before any response
    /// arrives for this request.
    pub async fn call(&self, msg_type: MessageType, body: Vec<u8>) -> Result<Message> {
        let request_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.lock_pending()?;
            if let Some(reason) = pending.dead.as_ref() {
                return Err(Error::Guest(format!("multiplex channel dead: {reason}")));
            }
            pending.slots.insert(request_id, Dispatch::Oneshot(tx));
        }

        let frame = build_frame(msg_type, request_id, &body);
        if let Err(e) = self.inner.writer.send(&frame) {
            let _ = self.remove_slot(request_id);
            return Err(e);
        }

        match rx.await {
            Ok(msg) => Ok(msg),
            Err(_) => {
                let reason = self
                    .lock_pending()
                    .ok()
                    .and_then(|pending| pending.dead.clone())
                    .unwrap_or_else(|| "reader dropped slot".to_string());
                Err(Error::Guest(format!(
                    "multiplex response for request_id={request_id} lost: {reason}"
                )))
            }
        }
    }

    /// Sends a streaming RPC and returns an mpsc receiver of frames.
    ///
    /// Frames that share the allocated `request_id` are forwarded to the
    /// returned [`mpsc::Receiver`] until the [`Terminator`] condition is
    /// met or the channel dies.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Guest`] if the channel is dead or if sending the
    /// initial request frame fails.
    pub async fn call_stream(
        &self,
        msg_type: MessageType,
        body: Vec<u8>,
        terminator: Terminator,
    ) -> Result<mpsc::Receiver<Message>> {
        let request_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);

        {
            let mut pending = self.lock_pending()?;
            if let Some(reason) = pending.dead.as_ref() {
                return Err(Error::Guest(format!("multiplex channel dead: {reason}")));
            }
            pending.slots.insert(
                request_id,
                Dispatch::Stream {
                    sender: tx,
                    terminator,
                },
            );
        }

        let frame = build_frame(msg_type, request_id, &body);
        if let Err(e) = self.inner.writer.send(&frame) {
            let _ = self.remove_slot(request_id);
            return Err(e);
        }

        Ok(rx)
    }

    /// Returns `true` if the reader thread has marked the channel dead.
    ///
    /// After this point, every [`call`](Self::call) and
    /// [`call_stream`](Self::call_stream) returns an error immediately.
    pub fn is_dead(&self) -> bool {
        match self.lock_pending() {
            Ok(pending) => pending.dead.is_some(),
            Err(_) => true,
        }
    }

    /// Waits for the reader thread to mark the channel dead, polling at
    /// a short interval.
    ///
    /// Intended for tests and shutdown coordination, not hot paths.
    pub async fn wait_dead(&self, interval: Duration) {
        loop {
            if self.is_dead() {
                return;
            }
            tokio::time::sleep(interval).await;
        }
    }

    fn remove_slot(&self, request_id: u32) -> Result<()> {
        let mut pending = self.lock_pending()?;
        pending.slots.remove(&request_id);
        Ok(())
    }

    fn lock_pending(&self) -> Result<std::sync::MutexGuard<'_, PendingTable>> {
        self.inner
            .pending
            .lock()
            .map_err(|_| Error::Guest("multiplex pending table poisoned".into()))
    }
}

/// Serializes a request/response/stream frame with the request_id prefix.
///
/// # Examples
///
/// ```ignore
/// let frame = build_frame(MessageType::Ping, 7, b"hello");
/// assert_eq!(&frame[5..9], &7u32.to_le_bytes());
/// ```
pub fn build_frame(msg_type: MessageType, request_id: u32, body: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(REQUEST_ID_PREFIX + body.len());
    payload.extend_from_slice(&request_id.to_le_bytes());
    payload.extend_from_slice(body);
    Message { msg_type, payload }.serialize()
}

/// Splits a multiplex payload into `(request_id, body)`.
///
/// Returns `None` if `payload` is shorter than [`REQUEST_ID_PREFIX`].
pub fn decode_payload(payload: &[u8]) -> Option<(u32, &[u8])> {
    if payload.len() < REQUEST_ID_PREFIX {
        return None;
    }
    let request_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Some((request_id, &payload[REQUEST_ID_PREFIX..]))
}

/// Decoded multiplex frame with request_id lifted out of `payload`.
///
/// Returned by [`read_multiplex_frame`] to keep the reader loop terse.
struct MultiplexFrame {
    msg_type: MessageType,
    request_id: u32,
    body: Vec<u8>,
}

/// Reads and deframes one multiplex message from the reader.
fn read_multiplex_frame<R: Read>(reader: &mut R) -> std::result::Result<MultiplexFrame, ReadError> {
    let msg = Message::read_from_sync(reader).map_err(ReadError::Protocol)?;
    let Some((request_id, body_slice)) = decode_payload(&msg.payload) else {
        return Err(ReadError::ShortPayload {
            msg_type: msg.msg_type,
            payload_len: msg.payload.len(),
        });
    };
    let body = body_slice.to_vec();
    Ok(MultiplexFrame {
        msg_type: msg.msg_type,
        request_id,
        body,
    })
}

enum ReadError {
    Protocol(ProtocolError),
    ShortPayload {
        msg_type: MessageType,
        payload_len: usize,
    },
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Protocol(e) => write!(f, "protocol: {e}"),
            ReadError::ShortPayload {
                msg_type,
                payload_len,
            } => write!(
                f,
                "multiplex frame too short for {msg_type:?}: {payload_len} bytes \
                 (need at least {REQUEST_ID_PREFIX})"
            ),
        }
    }
}

fn reader_loop(mut reader: Box<dyn Read + Send>, pending: Arc<Mutex<PendingTable>>) {
    loop {
        match read_multiplex_frame(&mut reader) {
            Ok(frame) => {
                let MultiplexFrame {
                    msg_type,
                    request_id,
                    body,
                } = frame;
                dispatch_frame(&pending, msg_type, request_id, body);
            }
            Err(e) => {
                mark_channel_dead(&pending, format!("reader exiting: {e}"));
                return;
            }
        }
    }
}

fn dispatch_frame(
    pending: &Arc<Mutex<PendingTable>>,
    msg_type: MessageType,
    request_id: u32,
    body: Vec<u8>,
) {
    debug!(
        "multiplex: dispatch msg_type={msg_type:?} request_id={request_id} body_len={}",
        body.len()
    );
    let mut guard = match pending.lock() {
        Ok(g) => g,
        Err(_) => {
            warn!("multiplex: pending table poisoned; dropping frame");
            return;
        }
    };

    let slot = guard.slots.remove(&request_id);
    let Some(dispatch) = slot else {
        warn!("multiplex: no pending slot for request_id={request_id} msg_type={msg_type:?}");
        return;
    };

    match dispatch {
        Dispatch::Oneshot(tx) => {
            let msg = Message {
                msg_type,
                payload: body,
            };
            let _ = tx.send(msg);
        }
        Dispatch::Stream { sender, terminator } => {
            let is_terminal = match terminator {
                Terminator::OnMessageType(t) => msg_type == t,
                Terminator::ChannelLifetime => false,
            };
            let msg = Message {
                msg_type,
                payload: body,
            };
            if let Err(e) = sender.try_send(msg) {
                debug!(
                    "multiplex: stream receiver full or closed for request_id={request_id}: {e}"
                );
            }
            if !is_terminal {
                guard
                    .slots
                    .insert(request_id, Dispatch::Stream { sender, terminator });
            }
        }
    }
}

fn mark_channel_dead(pending: &Arc<Mutex<PendingTable>>, reason: String) {
    let Ok(mut guard) = pending.lock() else {
        return;
    };
    if guard.dead.is_none() {
        debug!("multiplex: marking channel dead ({reason})");
        guard.dead = Some(reason);
    }
    guard.slots.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    struct FdFrameSender {
        stream: Mutex<UnixStream>,
    }

    impl FrameSender for FdFrameSender {
        fn send(&self, frame: &[u8]) -> Result<()> {
            let mut stream = self
                .stream
                .lock()
                .map_err(|_| Error::Guest("frame sender poisoned".into()))?;
            stream
                .write_all(frame)
                .map_err(|e| Error::Guest(format!("frame send failed: {e}")))?;
            Ok(())
        }
    }

    /// Simulates the guest-agent: reads framed requests, echoes a
    /// scripted reply for each request_id. Runs on its own thread so
    /// the test mirrors production threading.
    fn spawn_mock_guest(
        mut stream: UnixStream,
        scripted: Vec<MockReply>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut scripted = scripted.into_iter();
            loop {
                let Ok(msg) = Message::read_from_sync(&mut stream) else {
                    return;
                };
                let Some((request_id, _body)) = decode_payload(&msg.payload) else {
                    return;
                };
                let Some(reply) = scripted.next() else {
                    return;
                };
                for (reply_type, reply_body) in reply.frames {
                    let frame = build_frame(reply_type, request_id, &reply_body);
                    if stream.write_all(&frame).is_err() {
                        return;
                    }
                }
            }
        })
    }

    struct MockReply {
        frames: Vec<(MessageType, Vec<u8>)>,
    }

    fn mock_pair() -> (Box<dyn Read + Send>, Arc<dyn FrameSender>, UnixStream) {
        let (host_side, guest_side) = UnixStream::pair().expect("unix stream pair");
        let writer_stream = host_side.try_clone().expect("clone write half");
        let reader: Box<dyn Read + Send> = Box::new(host_side);
        let writer: Arc<dyn FrameSender> = Arc::new(FdFrameSender {
            stream: Mutex::new(writer_stream),
        });
        (reader, writer, guest_side)
    }

    #[test]
    fn build_frame_layout_has_request_id_prefix() {
        let frame = build_frame(MessageType::Ping, 0xDEAD_BEEF, b"xyz");
        let msg = Message::deserialize(&frame).unwrap();
        assert_eq!(msg.msg_type, MessageType::Ping);
        assert_eq!(msg.payload.len(), REQUEST_ID_PREFIX + 3);
        let (id, body) = decode_payload(&msg.payload).unwrap();
        assert_eq!(id, 0xDEAD_BEEF);
        assert_eq!(body, b"xyz");
    }

    #[test]
    fn decode_payload_rejects_short_frames() {
        assert!(decode_payload(&[]).is_none());
        assert!(decode_payload(&[0u8; 3]).is_none());
        let (id, body) = decode_payload(&[1, 0, 0, 0, 42]).unwrap();
        assert_eq!(id, 1);
        assert_eq!(body, &[42]);
    }

    #[tokio::test]
    async fn call_oneshot_round_trip() {
        let (reader, writer, guest) = mock_pair();
        let _guest_thread = spawn_mock_guest(
            guest,
            vec![MockReply {
                frames: vec![(MessageType::Pong, b"ack".to_vec())],
            }],
        );

        let channel = MultiplexChannel::new(reader, writer);
        let msg = tokio::time::timeout(
            Duration::from_secs(2),
            channel.call(MessageType::Ping, b"hello".to_vec()),
        )
        .await
        .expect("call did not time out")
        .expect("oneshot rpc");

        assert_eq!(msg.msg_type, MessageType::Pong);
        assert_eq!(msg.payload, b"ack");
    }

    #[tokio::test]
    async fn call_stream_terminates_on_message_type() {
        let (reader, writer, guest) = mock_pair();
        let _guest_thread = spawn_mock_guest(
            guest,
            vec![MockReply {
                frames: vec![
                    (MessageType::ExecOutputChunk, b"chunk-1".to_vec()),
                    (MessageType::ExecResponse, b"final".to_vec()),
                ],
            }],
        );

        let channel = MultiplexChannel::new(reader, writer);
        let mut rx = channel
            .call_stream(
                MessageType::ExecRequest,
                b"run".to_vec(),
                Terminator::OnMessageType(MessageType::ExecResponse),
            )
            .await
            .expect("stream rpc");

        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.msg_type, MessageType::ExecOutputChunk);

        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.msg_type, MessageType::ExecResponse);
    }

    #[tokio::test]
    async fn concurrent_calls_demux_by_request_id() {
        let (reader, writer, guest) = mock_pair();
        let mut replies = Vec::new();
        for index in 0..32 {
            replies.push(MockReply {
                frames: vec![(MessageType::Pong, format!("reply-{index}").into_bytes())],
            });
        }
        let _guest_thread = spawn_mock_guest(guest, replies);

        let channel = MultiplexChannel::new(reader, writer);
        let mut handles = Vec::new();
        for index in 0..32 {
            let channel = channel.clone();
            handles.push(tokio::spawn(async move {
                let body = format!("req-{index}").into_bytes();
                let msg = tokio::time::timeout(
                    Duration::from_secs(3),
                    channel.call(MessageType::Ping, body),
                )
                .await
                .expect("call timeout")
                .expect("call error");
                String::from_utf8(msg.payload).unwrap()
            }));
        }

        let mut seen = std::collections::HashSet::new();
        for handle in handles {
            seen.insert(handle.await.unwrap());
        }
        assert_eq!(seen.len(), 32);
        for index in 0..32 {
            assert!(seen.contains(&format!("reply-{index}")));
        }
    }

    #[tokio::test]
    async fn reader_death_fails_pending_calls() {
        let (reader, writer, guest) = mock_pair();
        drop(guest);

        let channel = MultiplexChannel::new(reader, writer);
        let err = tokio::time::timeout(
            Duration::from_secs(2),
            channel.call(MessageType::Ping, b"probe".to_vec()),
        )
        .await
        .expect("did not time out")
        .expect_err("expected channel-dead error");

        match err {
            Error::Guest(msg) => assert!(
                msg.contains("multiplex")
                    || msg.contains("lost")
                    || msg.contains("Broken pipe")
                    || msg.contains("frame send failed"),
                "unexpected error message: {msg}"
            ),
            other => panic!("expected Error::Guest, got {other:?}"),
        }
    }
}
