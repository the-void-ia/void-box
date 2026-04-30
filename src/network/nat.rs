//! Stateless address translation for SLIRP.
//!
//! Pure functions that map (guest-visible address, rules) → (host-side
//! `SocketAddr` to connect/bind to). No per-flow state lives here —
//! the flow table in `slirp.rs` owns that. Translation itself is a
//! function call.
//!
//! Mirrors passt's `fwd.c::nat_inbound` design: address rewrites are
//! pure functions of (address, rules), not per-flow state. Sets up the
//! shape for IPv6 dual-stack (Phase 6) and port-forwarding (Phase 5
//! Task 5.5).

use std::net::{Ipv4Addr, SocketAddr};

use ipnet::Ipv4Net;
use smoltcp::wire::Ipv4Address;

/// Transport protocol discriminant for a port-forwarding rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardProto {
    /// Transmission Control Protocol.
    Tcp,
    /// User Datagram Protocol.
    Udp,
}

/// One inbound port-forwarding entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortForward {
    /// Transport protocol; TCP or UDP.
    pub proto: ForwardProto,
    /// Host port to bind. Connections to `127.0.0.1:host_port` are
    /// proxied into the guest at `guest_port`.
    pub host_port: u16,
    /// Guest port the forwarded connection terminates at.
    pub guest_port: u16,
}

/// Outbound translation rules, derived once at `SlirpBackend`
/// construction.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// If `true`, guest connections to the SLIRP gateway IP map to
    /// `127.0.0.1` on the host. Today this is always `true`; left
    /// configurable so a future TAP backend can flip it off.
    pub gateway_loopback: bool,
    /// CIDRs the guest is not allowed to connect to. Outbound packets
    /// targeting these get `None` from [`translate_outbound`].
    pub deny_cidrs: Vec<Ipv4Net>,
    /// Inbound port forwards. Consulted by `SlirpBackend::new` to
    /// spawn host listeners; not used by [`translate_outbound`].
    pub port_forwards: Vec<PortForward>,
}

/// Translate an outbound packet's destination address.
///
/// Returns `Some(host_addr)` if the packet should be forwarded —
/// loopback for the gateway IP, otherwise the original IP. Returns
/// `None` if the destination is in the deny list.
///
/// # Examples
///
/// ```
/// use ipnet::Ipv4Net;
/// use smoltcp::wire::Ipv4Address;
/// use void_box::network::nat::{Rules, translate_outbound};
///
/// let rules = Rules {
///     gateway_loopback: true,
///     deny_cidrs: vec!["169.254.0.0/16".parse().unwrap()],
///     ..Default::default()
/// };
/// let gateway = Ipv4Address::new(10, 0, 2, 2);
///
/// // Gateway IP is rewritten to loopback.
/// let addr = translate_outbound(&rules, gateway, 80, gateway).unwrap();
/// assert_eq!(addr.ip().to_string(), "127.0.0.1");
///
/// // External IPs pass through unchanged.
/// let ext = Ipv4Address::new(8, 8, 8, 8);
/// let addr = translate_outbound(&rules, ext, 53, gateway).unwrap();
/// assert_eq!(addr.ip().to_string(), "8.8.8.8");
///
/// // Deny-listed IPs return None.
/// let metadata = Ipv4Address::new(169, 254, 169, 254);
/// assert!(translate_outbound(&rules, metadata, 80, gateway).is_none());
/// ```
pub fn translate_outbound(
    rules: &Rules,
    dst: Ipv4Address,
    dst_port: u16,
    gateway_ip: Ipv4Address,
) -> Option<SocketAddr> {
    let dst_ipv4 = Ipv4Addr::from(dst.0);

    // Deny-list check first — explicit block beats any other rule.
    for cidr in &rules.deny_cidrs {
        if cidr.contains(&dst_ipv4) {
            return None;
        }
    }

    let host_ip = if rules.gateway_loopback && dst == gateway_ip {
        Ipv4Addr::LOCALHOST
    } else {
        dst_ipv4
    };

    Some(SocketAddr::from((host_ip, dst_port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway() -> Ipv4Address {
        Ipv4Address::new(10, 0, 2, 2)
    }

    fn rules_basic() -> Rules {
        Rules {
            gateway_loopback: true,
            deny_cidrs: vec!["169.254.0.0/16".parse().unwrap()],
            ..Default::default()
        }
    }

    #[test]
    fn gateway_ip_maps_to_loopback() {
        let gw = gateway();
        let addr = translate_outbound(&rules_basic(), gw, 80, gw).unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 80);
    }

    #[test]
    fn external_ip_passes_through_unchanged() {
        let gw = gateway();
        let ext = Ipv4Address::new(8, 8, 8, 8);
        let addr = translate_outbound(&rules_basic(), ext, 53, gw).unwrap();
        assert_eq!(addr.ip().to_string(), "8.8.8.8");
        assert_eq!(addr.port(), 53);
    }

    #[test]
    fn deny_listed_ip_returns_none() {
        let gw = gateway();
        let metadata = Ipv4Address::new(169, 254, 169, 254);
        assert!(translate_outbound(&rules_basic(), metadata, 80, gw).is_none());
    }

    #[test]
    fn gateway_loopback_false_passes_gateway_through() {
        let gw = gateway();
        let rules = Rules {
            gateway_loopback: false,
            ..Default::default()
        };
        let addr = translate_outbound(&rules, gw, 443, gw).unwrap();
        assert_eq!(addr.ip().to_string(), "10.0.2.2");
        assert_eq!(addr.port(), 443);
    }

    #[test]
    fn empty_deny_list_allows_all() {
        let gw = gateway();
        let rules = Rules {
            gateway_loopback: false,
            deny_cidrs: vec![],
            ..Default::default()
        };
        let private = Ipv4Address::new(192, 168, 1, 1);
        let addr = translate_outbound(&rules, private, 22, gw).unwrap();
        assert_eq!(addr.ip().to_string(), "192.168.1.1");
    }
}
