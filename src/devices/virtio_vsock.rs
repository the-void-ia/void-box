//! virtio-vsock device for host-guest communication
//!
//! This module implements vsock (VM sockets) for communication between
//! the host and guest. The host connects to the guest using AF_VSOCK
//! with (guest_cid, port). Only one vhost-vsock backend per CID should
//! exist (VirtioVsockMmio sets the CID and owns the backend); this device
//! only performs the host-side connect using the same CID.

use std::io::{Read, Write};
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::guest::protocol::{ExecRequest, ExecResponse, Message, MessageType, TelemetryBatch};
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
}

impl VsockDevice {
    /// Create a new vsock device with the given CID (guest CID).
    pub fn new(cid: u32) -> Result<Self> {
        if cid < 3 {
            return Err(Error::Config(format!(
                "Invalid CID {}: must be >= 3 (0-2 reserved)",
                cid
            )));
        }

        info!("Creating vsock device with CID {} (host connects to guest via AF_VSOCK)", cid);

        Ok(Self { cid })
    }

    /// Get the CID for this device
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// Send an exec request to the guest agent and wait for response.
    ///
    /// Uses a VM0-style handshake: after connect, send Ping and wait for Pong
    /// before sending the first request. This confirms the guest agent is ready
    /// and avoids sending ExecRequest before the guest is in the read loop.
    pub async fn send_exec_request(&self, request: &ExecRequest) -> Result<ExecResponse> {
        // Short initial wait for guest kernel to boot and virtio-vsock to be probed.
        // The kernel typically boots in ~1.5s; the retry loop handles any remaining lag.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let mut delay = Duration::from_millis(100);
        let deadline = Instant::now() + Duration::from_secs(30);
        const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
        let mut attempt: u32 = 0;

        let mut stream = loop {
            if Instant::now() >= deadline {
                eprintln!("[vsock] deadline reached after {} connect/handshake attempts", attempt);
                warn!("vsock: deadline reached after {} connect/handshake attempts", attempt);
                return Err(Error::Guest(
                    "vsock: deadline reached (connect or handshake)".into(),
                ));
            }

            attempt += 1;

            let mut s = match self.connect_to_guest(GUEST_AGENT_PORT) {
                Ok(stream) => {
                    eprintln!("[vsock] attempt {} connect OK (cid={}, port={})", attempt, self.cid, GUEST_AGENT_PORT);
                    info!("vsock: attempt {} connect OK (cid={}, port={})", attempt, self.cid, GUEST_AGENT_PORT);
                    stream
                }
                Err(e) => {
                    eprintln!("[vsock] attempt {} connect failed: {} (retry in {:?})", attempt, e, delay);
                    warn!("vsock: attempt {} connect failed: {} (retry in {:?})", attempt, e, delay);
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                    continue;
                }
            };

            // VM0-style handshake: Ping -> Pong before first request
            if let Err(e) = s.set_read_timeout(Some(HANDSHAKE_TIMEOUT)) {
                warn!("vsock: attempt {} set_read_timeout failed: {}", attempt, e);
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                continue;
            }
            let ping_msg = Message {
                msg_type: MessageType::Ping,
                payload: vec![],
            };
            let ping_bytes = ping_msg.serialize();
            if s.write_all(&ping_bytes).is_err() {
                warn!("vsock: attempt {} handshake failed to send Ping", attempt);
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                continue;
            }
            info!("vsock: attempt {} Ping sent, waiting for Pong (timeout {:?})", attempt, HANDSHAKE_TIMEOUT);
            match Message::read_from_sync(&mut s) {
                Ok(msg) if msg.msg_type == MessageType::Pong => {
                    info!("vsock: handshake OK (Pong received)");
                    break s;
                }
                Ok(msg) => {
                    warn!("vsock: attempt {} handshake unexpected message type {:?}", attempt, msg.msg_type);
                }
                Err(e) => {
                    warn!("vsock: attempt {} handshake read Pong failed: {}", attempt, e);
                }
            }
            tokio::time::sleep(delay).await;
            delay = std::cmp::min(delay * 2, Duration::from_secs(2));
        };

        // Set a generous read timeout for the response (commands should finish
        // within 60s; prevents hanging forever if the guest gets stuck).
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));

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
        // Read response
        let response_msg = Message::read_from_sync(&mut stream)?;

        if response_msg.msg_type != MessageType::ExecResponse {
            return Err(Error::Guest(format!(
                "Unexpected response type: {:?}",
                response_msg.msg_type
            )));
        }

        let response: ExecResponse = serde_json::from_slice(&response_msg.payload)?;
        info!("vsock: ExecResponse received exit_code={}", response.exit_code);

        Ok(response)
    }

    /// Open a persistent telemetry subscription to the guest agent.
    ///
    /// Connects to the guest, performs a Ping/Pong handshake, sends a
    /// SubscribeTelemetry message, then reads TelemetryData messages in a
    /// loop, calling `on_batch` for each one. Returns when the connection drops.
    pub async fn subscribe_telemetry<F>(&self, mut on_batch: F) -> Result<()>
    where
        F: FnMut(TelemetryBatch) + Send + 'static,
    {
        // Wait for guest boot (shorter than exec since the VM is already up by now)
        let mut delay = Duration::from_millis(500);
        let deadline = Instant::now() + Duration::from_secs(30);
        const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

        let mut stream = loop {
            if Instant::now() >= deadline {
                return Err(Error::Guest(
                    "Telemetry subscription: deadline reached connecting to guest".into(),
                ));
            }

            let mut s = match self.connect_to_guest(GUEST_AGENT_PORT) {
                Ok(s) => s,
                Err(_) => {
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                    continue;
                }
            };

            // Handshake: Ping -> Pong
            if s.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).is_err() {
                tokio::time::sleep(delay).await;
                continue;
            }
            let ping_msg = Message {
                msg_type: MessageType::Ping,
                payload: vec![],
            };
            if s.write_all(&ping_msg.serialize()).is_err() {
                tokio::time::sleep(delay).await;
                continue;
            }
            match Message::read_from_sync(&mut s) {
                Ok(msg) if msg.msg_type == MessageType::Pong => break s,
                _ => {
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, Duration::from_secs(2));
                    continue;
                }
            }
        };

        // Send SubscribeTelemetry
        let sub_msg = Message {
            msg_type: MessageType::SubscribeTelemetry,
            payload: vec![],
        };
        stream
            .write_all(&sub_msg.serialize())
            .map_err(|e| Error::Guest(format!("Failed to send SubscribeTelemetry: {}", e)))?;

        info!("Telemetry subscription active (cid={})", self.cid);

        // Clear read timeout - telemetry messages come every 2s
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));

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
                warn!("Unexpected message type in telemetry stream: {:?}", msg.msg_type);
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
        let socket_fd = unsafe {
            libc::socket(
                libc::AF_VSOCK,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
            )
        };

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
            unsafe { libc::close(socket_fd); }
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
    fn set_read_timeout(&self, duration: Option<Duration>) -> std::io::Result<()> {
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
        let n = unsafe {
            libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = unsafe {
            libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len())
        };
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
        unsafe { libc::close(self.fd); }
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
