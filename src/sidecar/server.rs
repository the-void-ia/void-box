//! Guest-facing HTTP server for the void-box sidecar.
//!
//! Spawns a bare-metal tokio TCP listener that exposes inbox, intent, context,
//! and health endpoints to the agent running inside the VM.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tracing::{debug, error, info};

use crate::error::ApiError;
use crate::sidecar::state::{IntentRejection, SidecarState};
use crate::sidecar::types::*;

const SIDECAR_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_REQUEST_BYTES: usize = 65_536;
const MAX_INTENT_PAYLOAD_BYTES: usize = 4096;

/// Handle returned by [`start_sidecar`]. Provides async methods to interact
/// with the sidecar state from the host side, and to shut down the server.
pub struct SidecarHandle {
    addr: SocketAddr,
    state: Arc<Mutex<SidecarState>>,
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl SidecarHandle {
    /// The address the sidecar HTTP server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Load a new inbox snapshot (called by the orchestrator after each iteration).
    pub async fn load_inbox(&self, snapshot: InboxSnapshot) {
        self.state.lock().await.load_inbox(snapshot);
    }

    /// Drain all buffered intents (called by the orchestrator to collect agent intents).
    pub async fn drain_intents(&self) -> Vec<StampedIntent> {
        self.state.lock().await.drain_intents()
    }

    /// Push a single message entry into the current inbox.
    pub async fn push_message(&self, entry: InboxEntry) {
        let mut state = self.state.lock().await;
        if let Some(inbox) = state.inbox_mut() {
            inbox.entries.push(entry);
        }
    }

    /// Return `(buffer_depth, inbox_version)`.
    pub async fn state_snapshot(&self) -> (usize, u64) {
        let state = self.state.lock().await;
        (state.buffer_depth(), state.inbox_version())
    }

    /// Signal the sidecar to shut down (non-blocking, does not await task completion).
    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Gracefully shut down the sidecar HTTP server and await task completion.
    pub async fn stop(self) {
        self.signal_shutdown();
        let _ = self.task.await;
    }
}

/// Start the sidecar HTTP server. Returns a [`SidecarHandle`] that provides
/// host-side access to the sidecar state and the ability to stop the server.
pub async fn start_sidecar(
    run_id: &str,
    execution_id: &str,
    candidate_id: &str,
    peers: Vec<String>,
    bind_addr: SocketAddr,
) -> std::io::Result<SidecarHandle> {
    let state = Arc::new(Mutex::new(SidecarState::new(
        run_id,
        execution_id,
        candidate_id,
        peers,
    )));
    let listener = TcpListener::bind(bind_addr).await?;
    let addr = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let server_state = state.clone();
    let task = tokio::spawn(async move {
        run_server(listener, server_state, shutdown_rx).await;
    });

    info!(
        run_id,
        addr = %addr,
        sidecar_version = SIDECAR_VERSION,
        "sidecar started"
    );

    Ok(SidecarHandle {
        addr,
        state,
        shutdown_tx,
        task,
    })
}

async fn run_server(
    listener: TcpListener,
    state: Arc<Mutex<SidecarState>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        let st = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_stream(stream, peer, st).await {
                                debug!("sidecar connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        error!("sidecar accept error: {e}");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let st = state.lock().await;
                    info!(
                        run_id = %st.run_id(),
                        buffered_intents = st.buffer_depth(),
                        uptime_ms = st.uptime_ms(),
                        "sidecar stopping"
                    );
                    drop(st);
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP handling
// ---------------------------------------------------------------------------

async fn handle_stream(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: Arc<Mutex<SidecarState>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read the full request (headers + body) respecting Content-Length.
    let mut buf = vec![0u8; MAX_REQUEST_BYTES];
    let mut total = 0usize;

    // Read until we have at least the headers (terminated by \r\n\r\n).
    let header_end;
    loop {
        if total >= buf.len() {
            send_response(
                &mut stream,
                "413 Payload Too Large",
                &ApiError::payload_too_large("request too large").to_json(),
            )
            .await?;
            return Ok(());
        }
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            return Ok(());
        }
        total += n;

        if let Some(pos) = find_header_end(&buf[..total]) {
            header_end = pos;
            break;
        }
    }

    let headers_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let body_start = header_end + 4; // skip \r\n\r\n

    // Parse Content-Length from headers
    let content_length = parse_content_length(&headers_str);

    // Read remaining body bytes if needed
    if content_length > 0 {
        let needed = body_start + content_length;
        if needed > MAX_REQUEST_BYTES {
            send_response(
                &mut stream,
                "413 Payload Too Large",
                &ApiError::payload_too_large("request too large").to_json(),
            )
            .await?;
            return Ok(());
        }
        while total < needed {
            if total >= buf.len() {
                send_response(
                    &mut stream,
                    "413 Payload Too Large",
                    &ApiError::payload_too_large("request too large").to_json(),
                )
                .await?;
                return Ok(());
            }
            let n = stream.read(&mut buf[total..]).await?;
            if n == 0 {
                break;
            }
            total += n;
        }
    }

    let body = if body_start < total {
        String::from_utf8_lossy(&buf[body_start..total]).to_string()
    } else {
        String::new()
    };

    // Parse request line
    let first_line = headers_str.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("");

    let (path, query) = raw_path
        .split_once('?')
        .map_or((raw_path, None), |(p, q)| (p, Some(q)));

    // Parse Idempotency-Key header
    let idempotency_key = parse_header(&headers_str, "idempotency-key");

    let (status, response_body) =
        route(method, path, query, &body, idempotency_key, peer, &state).await;
    send_response(&mut stream, status, &response_body).await?;

    Ok(())
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("content-length") {
            let Ok(len) = value.trim().parse::<usize>() else {
                continue;
            };
            return len;
        }
    }
    0
}

fn parse_header(headers: &str, name: &str) -> Option<String> {
    let lower_name = name.to_lowercase();
    for line in headers.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().to_lowercase() == lower_name {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn parse_query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            Some(v.to_string())
        } else {
            None
        }
    })
}

async fn send_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        body.len(),
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

async fn route(
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &str,
    idempotency_key: Option<String>,
    peer: SocketAddr,
    state: &Arc<Mutex<SidecarState>>,
) -> (&'static str, String) {
    match (method, path) {
        ("GET", "/v1/health") => handle_health(peer, state).await,
        ("GET", "/v1/inbox") => handle_get_inbox(query, state).await,
        ("POST", "/v1/intents") => handle_post_intents(body, idempotency_key, state).await,
        ("GET", "/v1/context") => handle_get_context(state).await,
        ("GET", "/v1/signals") => (
            "501 Not Implemented",
            ApiError::internal("signals endpoint not yet implemented").to_json(),
        ),
        _ => (
            "404 Not Found",
            ApiError::not_found(format!("no route for {method} {path}")).to_json(),
        ),
    }
}

async fn handle_health(
    peer: SocketAddr,
    state: &Arc<Mutex<SidecarState>>,
) -> (&'static str, String) {
    let st = state.lock().await;
    debug!(
        run_id = %st.run_id(),
        client_addr = %peer,
        "health check served"
    );
    let health = SidecarHealth {
        status: "ok".into(),
        sidecar_version: SIDECAR_VERSION.into(),
        run_id: st.run_id().into(),
        buffer_depth: st.buffer_depth(),
        inbox_version: st.inbox_version(),
        uptime_ms: st.uptime_ms(),
    };
    (
        "200 OK",
        serde_json::to_string(&health).unwrap_or_else(|_| "{}".into()),
    )
}

async fn handle_get_inbox(
    query: Option<&str>,
    state: &Arc<Mutex<SidecarState>>,
) -> (&'static str, String) {
    let since = parse_query_param(query, "since").and_then(|v| v.parse::<u64>().ok());
    let st = state.lock().await;
    let inbox = st.get_inbox(since);
    (
        "200 OK",
        serde_json::to_string(&inbox).unwrap_or_else(|_| "{}".into()),
    )
}

async fn handle_post_intents(
    body: &str,
    idempotency_key: Option<String>,
    state: &Arc<Mutex<SidecarState>>,
) -> (&'static str, String) {
    // Try parsing as array first, then as single object
    let intents: Vec<SubmittedIntent> = if body.trim_start().starts_with('[') {
        match serde_json::from_str::<Vec<SubmittedIntent>>(body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    "400 Bad Request",
                    ApiError::invalid_params(format!("invalid JSON: {e}")).to_json(),
                );
            }
        }
    } else {
        match serde_json::from_str::<SubmittedIntent>(body) {
            Ok(v) => vec![v],
            Err(e) => {
                return (
                    "400 Bad Request",
                    ApiError::invalid_params(format!("invalid JSON: {e}")).to_json(),
                );
            }
        }
    };

    // Check raw body size for oversized payload
    if body.len() > MAX_INTENT_PAYLOAD_BYTES {
        // Only reject if individual intents are too large; for batches, the
        // per-intent check inside accept_intent will catch it. But if the
        // whole body exceeds the limit and it's a single intent, reject early.
        if intents.len() == 1 {
            return (
                "413 Payload Too Large",
                ApiError::payload_too_large("intent payload exceeds 4096 bytes").to_json(),
            );
        }
    }

    let is_batch = body.trim_start().starts_with('[');
    let mut st = state.lock().await;
    let mut stamped_results: Vec<StampedIntent> = Vec::with_capacity(intents.len());

    for (i, intent) in intents.into_iter().enumerate() {
        // For batches, only the first intent gets the idempotency key
        let key = if i == 0 {
            idempotency_key.clone()
        } else {
            None
        };

        match st.accept_intent(intent, key) {
            Ok(stamped) => stamped_results.push(stamped),
            Err(IntentRejection::PayloadTooLarge) => {
                return (
                    "413 Payload Too Large",
                    ApiError::payload_too_large("intent payload exceeds 4096 bytes").to_json(),
                );
            }
            Err(IntentRejection::MaxPerIteration) => {
                return (
                    "429 Too Many Requests",
                    ApiError::too_many_requests("max intents per iteration reached").to_json(),
                );
            }
            Err(IntentRejection::RateLimited) => {
                return (
                    "429 Too Many Requests",
                    ApiError::too_many_requests("rate limited").to_json(),
                );
            }
        }
    }

    let body = if is_batch {
        serde_json::to_string(&stamped_results).unwrap_or_else(|_| "[]".into())
    } else {
        serde_json::to_string(&stamped_results[0]).unwrap_or_else(|_| "{}".into())
    };

    ("201 Created", body)
}

async fn handle_get_context(state: &Arc<Mutex<SidecarState>>) -> (&'static str, String) {
    let st = state.lock().await;
    let ctx = SidecarContext {
        execution_id: st.execution_id().into(),
        candidate_id: st.candidate_id().into(),
        iteration: st.current_iteration(),
        role: "candidate".into(),
        peers: st.peers().to_vec(),
        sidecar_version: SIDECAR_VERSION.into(),
    };
    (
        "200 OK",
        serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sidecar_starts_and_stops() {
        let handle = start_sidecar(
            "run-1",
            "exec-1",
            "cand-1",
            vec!["cand-2".into()],
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        let addr = handle.addr();
        assert_ne!(addr.port(), 0);
        handle.stop().await;
    }

    #[tokio::test]
    async fn sidecar_state_snapshot() {
        let handle = start_sidecar(
            "run-1",
            "exec-1",
            "cand-1",
            vec![],
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();

        let (depth, version) = handle.state_snapshot().await;
        assert_eq!(depth, 0);
        assert_eq!(version, 0);

        handle
            .load_inbox(InboxSnapshot {
                version: 5,
                execution_id: "exec-1".into(),
                candidate_id: "cand-1".into(),
                iteration: 1,
                entries: vec![],
            })
            .await;

        let (depth, version) = handle.state_snapshot().await;
        assert_eq!(depth, 0);
        assert_eq!(version, 5);

        handle.stop().await;
    }
}
