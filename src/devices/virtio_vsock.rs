//! virtio-vsock device for host-guest communication
//!
//! This module implements vsock (VM sockets) for communication between
//! the host and guest. It uses the vhost-vsock kernel module for the
//! data plane and communicates via a Context ID (CID).

use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

use tracing::{debug, info};

use crate::guest::protocol::{ExecRequest, ExecResponse, Message, MessageType};
use crate::{Error, Result};

/// vsock port used by the guest agent
pub const GUEST_AGENT_PORT: u32 = 1234;

/// Reserved CIDs
pub const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;
pub const VMADDR_CID_HYPERVISOR: u32 = 0;
pub const VMADDR_CID_LOCAL: u32 = 1;
pub const VMADDR_CID_HOST: u32 = 2;

/// Vsock device for host-guest communication
pub struct VsockDevice {
    /// Context ID for this VM
    cid: u32,
    /// vhost-vsock device fd (if using kernel vhost)
    vhost_fd: Option<RawFd>,
}

impl VsockDevice {
    /// Create a new vsock device with the given CID
    pub fn new(cid: u32) -> Result<Self> {
        if cid < 3 {
            return Err(Error::Config(format!(
                "Invalid CID {}: must be >= 3 (0-2 reserved)",
                cid
            )));
        }

        info!("Creating vsock device with CID {}", cid);

        // Try to open vhost-vsock device
        let vhost_fd = match Self::setup_vhost_vsock(cid) {
            Ok(fd) => {
                debug!("Opened vhost-vsock device");
                Some(fd)
            }
            Err(e) => {
                debug!("vhost-vsock not available: {}, falling back to userspace", e);
                None
            }
        };

        Ok(Self { cid, vhost_fd })
    }

    /// Set up vhost-vsock kernel module
    fn setup_vhost_vsock(cid: u32) -> Result<RawFd> {
        use nix::fcntl::{open, OFlag};
        use nix::sys::stat::Mode;

        // Open /dev/vhost-vsock
        let fd = open(
            Path::new("/dev/vhost-vsock"),
            OFlag::O_RDWR | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::Device(format!("Failed to open /dev/vhost-vsock: {}", e)))?;

        // Set the CID using ioctl
        // VHOST_VSOCK_SET_GUEST_CID = 0x4008AF60
        const VHOST_VSOCK_SET_GUEST_CID: u64 = 0x4008AF60;

        let cid_val: u64 = cid as u64;
        let ret = unsafe {
            libc::ioctl(fd.as_raw_fd(), VHOST_VSOCK_SET_GUEST_CID as libc::c_ulong, &cid_val)
        };

        if ret < 0 {
            return Err(Error::Device(format!(
                "Failed to set vsock CID: {}",
                std::io::Error::last_os_error()
            )));
        }

        Ok(fd.as_raw_fd())
    }

    /// Get the CID for this device
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// Send an exec request to the guest agent and wait for response
    pub async fn send_exec_request(&self, request: &ExecRequest) -> Result<ExecResponse> {
        // Connect to guest agent via vsock
        let mut stream = self.connect_to_guest(GUEST_AGENT_PORT)?;

        // Serialize and send request
        let message = Message {
            msg_type: MessageType::ExecRequest,
            payload: serde_json::to_vec(request)?,
        };

        let msg_bytes = message.serialize();
        stream
            .write_all(&msg_bytes)
            .map_err(|e| Error::Guest(format!("Failed to send request: {}", e)))?;

        debug!("Sent exec request: {:?}", request);

        // Read response
        let response_msg = Message::read_from_sync(&mut stream)?;

        if response_msg.msg_type != MessageType::ExecResponse {
            return Err(Error::Guest(format!(
                "Unexpected response type: {:?}",
                response_msg.msg_type
            )));
        }

        let response: ExecResponse = serde_json::from_slice(&response_msg.payload)?;
        debug!("Received exec response: exit_code={}", response.exit_code);

        Ok(response)
    }

    /// Connect to a port on the guest
    fn connect_to_guest(&self, port: u32) -> Result<VsockStream> {
        // For userspace vsock, we'd use a Unix socket or similar
        // For now, use a placeholder that would be replaced with actual vsock
        VsockStream::connect(self.cid, port)
    }
}

impl Drop for VsockDevice {
    fn drop(&mut self) {
        if let Some(fd) = self.vhost_fd {
            unsafe {
                libc::close(fd);
            }
        }
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
