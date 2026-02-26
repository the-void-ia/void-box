//! virtio-vsock device for host-guest communication
//!
//! This module implements vsock (VM sockets) for communication between
//! the host and guest. The host connects to the guest using AF_VSOCK
//! with (guest_cid, port). Only one vhost-vsock backend per CID should
//! exist (VirtioVsockMmio sets the CID and owns the backend); this device
//! only performs the host-side connect using the same CID.

use std::io::{Read, Write};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::guest::protocol::{
    ExecOutputChunk, ExecRequest, ExecResponse, Message, MessageType, MkdirPRequest,
    MkdirPResponse, TelemetryBatch, TelemetrySubscribeRequest, WriteFileRequest, WriteFileResponse,
};
use crate::{Error, Result};

/// vsock port used by the guest agent
pub const GUEST_AGENT_PORT: u32 = 1234;

/// Reserved CIDs (see vsock(7))
pub const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;
pub const VMADDR_CID_HYPERVISOR: u32 = 0;
pub const VMADDR_CID_LOCAL: u32 = 1;
pub const VMADDR_CID_HOST: u32 = 2;

/// Vsock device for host-guest communication (host side only).
/// Does not open /dev/vhost-vsock; the VirtioVsockMmio device is the single
/// vhost-vsock backend for this VM's CID.
pub struct VsockDevice {
    /// Context ID for this VM (guest CID; host uses CID 2)
    cid: u32,
    /// Tracks whether the cold-boot wait has already been applied.
    boot_wait_done: AtomicBool,
    /// 32-byte session secret for vsock authentication.
    /// Sent as the Ping payload; guest validates against its cmdline secret.
    session_secret: [u8; 32],
}

impl VsockDevice {
    /// Create a new vsock device with the given CID and session secret.
    pub fn new(cid: u32) -> Result<Self> {
        Self::with_secret(cid, [0u8; 32])
    }

    /// Create a new vsock device with the given CID and session secret.
    pub fn with_secret(cid: u32, session_secret: [u8; 32]) -> Result<Self> {
        if cid < 3 {
            return Err(Error::Config(format!(
                "Invalid CID {}: must be >= 3 (0-2 reserved)",
                cid
            )));
        }

        info!(
            "Creating vsock device with CID {} (host connects to guest via AF_VSOCK)",
            cid
        );

        Ok(Self {
            cid,
            boot_wait_done: AtomicBool::new(false),
            session_secret,
        })
    }

    /// Get the CID for this device
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// Send an exec request to the guest agent and wait for response.
    ///
    /// Uses a connect handshake: after connect, send Ping and wait for Pong
    /// before sending the first request. This confirms the guest agent is ready
    /// and avoids sending ExecRequest before the guest is in the read loop.
    pub async fn send_exec_request(&self, request: &ExecRequest) -> Result<ExecResponse> {
        let mut stream = self
            .connect_with_handshake(Duration::from_secs(3), "exec")
            .await?;

        // Set the read timeout for the response.
        // Use the per-request timeout if provided, otherwise default to 20 minutes.
        // LLM inference (especially with local models via Ollama on CPU) can take
        // 10+ minutes per turn for complex prompts with tool definitions.
        // timeout_secs == Some(0) means service mode: wait forever (no timeout).
        if request.timeout_secs == Some(0) {
            let _ = stream.set_read_timeout(None); // Service mode: wait forever
        } else {
            let timeout = request
                .timeout_secs
                .filter(|&s| s > 0)
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(1200));
            let _ = stream.set_read_timeout(Some(timeout));
        }

        // Serialize and send request
        let message = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        };

        let msg_bytes = message.serialize();
        stream
            .write_all(&msg_bytes)
            .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

        info!("vsock: sent ExecRequest, waiting for ExecResponse");
        // Read messages in a loop, discarding streaming ExecOutputChunk
        // messages until we get the final ExecResponse. The guest-agent
        // always streams stdout/stderr chunks during execution.
        loop {
            let msg = Message::read_from_sync(&mut stream)?;
            match msg.msg_type {
                MessageType::ExecOutputChunk => {
                    // Discard streaming chunks in non-streaming mode
                    continue;
                }
                MessageType::ExecResponse => {
                    let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                    info!(
                        "vsock: ExecResponse received exit_code={}",
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
    ///
    /// Calls `on_chunk` for each `ExecOutputChunk` the guest sends before the
    /// final `ExecResponse`. The final response still contains full accumulated
    /// stdout/stderr for backward compatibility.
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

        // timeout_secs == Some(0) means service mode: wait forever (no timeout).
        if request.timeout_secs == Some(0) {
            let _ = stream.set_read_timeout(None); // Service mode: wait forever
        } else {
            let timeout = request
                .timeout_secs
                .filter(|&s| s > 0)
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(1200));
            let _ = stream.set_read_timeout(Some(timeout));
        }

        let message = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        };

        let msg_bytes = message.serialize();
        stream
            .write_all(&msg_bytes)
            .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

        info!("vsock: sent ExecRequest (streaming), waiting for chunks + ExecResponse");

        // Read messages in a loop until we get the final ExecResponse
        loop {
            let msg = Message::read_from_sync(&mut stream)?;
            match msg.msg_type {
                MessageType::ExecOutputChunk => {
                    if let Ok(chunk) = serde_json::from_slice::<ExecOutputChunk>(&msg.payload) {
                        on_chunk(chunk);
                    }
                }
                MessageType::ExecResponse => {
                    let response: ExecResponse = serde_json::from_slice(&msg.payload)?;
                    info!(
                        "vsock: ExecResponse received (streaming) exit_code={}",
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

    /// Write a file to the guest filesystem using the native WriteFile protocol.
    ///
    /// This sends a WriteFile message directly to the guest-agent, which writes
    /// the file in Rust without needing `sh`, `echo`, or `base64`. Parent
    /// directories are created automatically. The write runs as root in the
    /// guest (appropriate for host-initiated provisioning like skill files).
    pub async fn send_write_file(&self, path: &str, content: &[u8]) -> Result<WriteFileResponse> {
        let request = WriteFileRequest {
            path: path.to_string(),
            content: content.to_vec(),
            create_parents: true,
        };

        let mut stream = self
            .connect_with_handshake(Duration::from_secs(3), "write-file")
            .await?;

        // Set generous read timeout
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));

        let message = Message {
            msg_type: MessageType::WriteFile,
            payload: serde_json::to_vec(&request)?,
        };
        stream
            .write_all(&message.serialize())
            .map_err(|e| Error::Guest(format!("Failed to send WriteFile: {}", e)))?;

        let response_msg = Message::read_from_sync(&mut stream)?;
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

        let response_msg = Message::read_from_sync(&mut stream)?;
        if response_msg.msg_type != MessageType::MkdirPResponse {
            return Err(Error::Guest(format!(
                "Unexpected response type for MkdirP: {:?}",
                response_msg.msg_type
            )));
        }

        let response: MkdirPResponse = serde_json::from_slice(&response_msg.payload)?;
        Ok(response)
    }

    /// Connect to the guest agent and perform a Ping/Pong handshake.
    /// Shared helper for all request flows.
    async fn connect_with_handshake(
        &self,
        handshake_timeout: Duration,
        context: &str,
    ) -> Result<VsockStream> {
        // Wait for guest kernel boot once per VsockDevice lifetime.
        // Later calls rely on retry/backoff without paying a fixed delay.
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
                    "vsock[{context}]: deadline reached after {} connect/handshake attempts",
                    attempt
                );
                return Err(Error::Guest(
                    "vsock: deadline reached (connect or handshake)".into(),
                ));
            }

            attempt += 1;

            let mut s = match self.connect_to_guest(GUEST_AGENT_PORT) {
                Ok(stream) => {
                    debug!(
                        "vsock[{context}]: attempt {} connect OK (cid={}, port={})",
                        attempt, self.cid, GUEST_AGENT_PORT
                    );
                    stream
                }
                Err(e) => {
                    debug!(
                        "vsock[{context}]: attempt {} connect failed: {} (retry in {:?})",
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
                    "vsock[{context}]: attempt {} set_read_timeout failed: {}",
                    attempt, e
                );
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                continue;
            }
            // Ping payload carries the 32-byte session secret for authentication.
            // The guest-agent validates this against the secret from /proc/cmdline.
            let ping_msg = Message {
                msg_type: MessageType::Ping,
                payload: self.session_secret.to_vec(),
            };
            if s.write_all(&ping_msg.serialize()).is_err() {
                debug!("vsock[{context}]: attempt {} failed to send Ping", attempt);
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                continue;
            }
            match Message::read_from_sync(&mut s) {
                Ok(msg) if msg.msg_type == MessageType::Pong => {
                    debug!("vsock[{context}]: handshake OK");
                    return Ok(s);
                }
                Ok(msg) => {
                    debug!(
                        "vsock[{context}]: attempt {} unexpected handshake message: {:?}",
                        attempt, msg.msg_type
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                }
                Err(e) => {
                    debug!(
                        "vsock[{context}]: attempt {} handshake read failed: {}",
                        attempt, e
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                }
            }
        }
    }

    /// Open a persistent telemetry subscription to the guest agent.
    ///
    /// Connects to the guest, performs a Ping/Pong handshake, sends a
    /// SubscribeTelemetry message with the given options, then reads
    /// TelemetryData messages in a loop, calling `on_batch` for each one.
    /// Returns when the connection drops.
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

        // Send SubscribeTelemetry with subscription options
        let sub_msg = Message {
            msg_type: MessageType::SubscribeTelemetry,
            payload: serde_json::to_vec(opts).unwrap_or_default(),
        };
        stream
            .write_all(&sub_msg.serialize())
            .map_err(|e| Error::Guest(format!("Failed to send SubscribeTelemetry: {}", e)))?;

        info!(
            "Telemetry subscription active (cid={}, interval={}ms)",
            self.cid, opts.interval_ms
        );

        // Set read timeout with headroom above the collection interval
        let read_timeout_ms = opts.interval_ms.max(1000) * 5;
        let _ = stream.set_read_timeout(Some(Duration::from_millis(read_timeout_ms)));

        // Read TelemetryData messages in a loop
        loop {
            let msg = match Message::read_from_sync(&mut stream) {
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

    /// Connect to a port on the guest
    fn connect_to_guest(&self, port: u32) -> Result<VsockStream> {
        // For userspace vsock, we'd use a Unix socket or similar
        // For now, use a placeholder that would be replaced with actual vsock
        VsockStream::connect(self.cid, port)
    }
}

/// Vsock stream wrapper
pub struct VsockStream {
    fd: RawFd,
}

impl VsockStream {
    /// Connect to a vsock endpoint
    pub fn connect(cid: u32, port: u32) -> Result<Self> {
        // Try to create a vsock socket
        let socket_fd =
            unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };

        if socket_fd < 0 {
            return Err(Error::Guest(format!(
                "Failed to create vsock socket: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Set up sockaddr_vm
        #[repr(C)]
        struct SockaddrVm {
            svm_family: libc::sa_family_t,
            svm_reserved1: u16,
            svm_port: u32,
            svm_cid: u32,
            svm_zero: [u8; 4],
        }

        let addr = SockaddrVm {
            svm_family: libc::AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: cid,
            svm_zero: [0; 4],
        };

        let ret = unsafe {
            libc::connect(
                socket_fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<SockaddrVm>() as libc::socklen_t,
            )
        };

        if ret < 0 {
            unsafe {
                libc::close(socket_fd);
            }
            return Err(Error::Guest(format!(
                "Failed to connect to vsock {}:{}: {}",
                cid,
                port,
                std::io::Error::last_os_error()
            )));
        }

        Ok(Self { fd: socket_fd })
    }

    /// Set read timeout (e.g. for handshake). Pass `None` for blocking.
    pub fn set_read_timeout(&self, duration: Option<Duration>) -> std::io::Result<()> {
        let tv = match duration {
            None => libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            Some(d) => libc::timeval {
                tv_sec: d.as_secs() as i64,
                tv_usec: d.subsec_micros() as i64,
            },
        };
        let ret = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Read for VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cid_validation() {
        assert!(VsockDevice::new(0).is_err());
        assert!(VsockDevice::new(1).is_err());
        assert!(VsockDevice::new(2).is_err());
        // CID 3+ should be valid (may fail if /dev/vhost-vsock unavailable)
    }

    #[test]
    fn test_reserved_cids() {
        assert_eq!(VMADDR_CID_HYPERVISOR, 0);
        assert_eq!(VMADDR_CID_LOCAL, 1);
        assert_eq!(VMADDR_CID_HOST, 2);
    }
}
