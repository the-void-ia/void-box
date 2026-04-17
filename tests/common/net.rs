use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};

pub fn localhost_ephemeral_addr() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
}

/// Reserve a localhost ephemeral port by binding a listener and keeping it
/// alive. Returns the bound address and the listener itself so the caller can
/// hand the listener off to the server that will actually serve on it —
/// avoiding the TOCTOU window where a bind-then-close probe releases the port
/// before the real server can reclaim it.
pub fn reserve_localhost_listener() -> (SocketAddr, TcpListener) {
    let listener = TcpListener::bind(localhost_ephemeral_addr()).unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, listener)
}
