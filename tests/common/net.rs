use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};

pub fn localhost_ephemeral_addr() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
}

pub fn reserve_localhost_addr() -> SocketAddr {
    let listener = TcpListener::bind(localhost_ephemeral_addr()).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}
