//! SSRF guard for the proxy's upstream re-origination (RFC-0002 R3).
//!
//! The proxy connects upstream on the guest's behalf, so it must not be steered
//! into an internal target. A name the guest can influence — or a public name an
//! attacker has rebound — could resolve to a host-internal address (the cloud
//! metadata endpoint, an RFC-1918 service, the host loopback). Leaving upstream
//! resolution to `reqwest`'s default resolver would connect to whatever the name
//! resolves to, with no internal-range check.
//!
//! The proxy's upstream client resolves names through this guard instead. It
//! resolves each upstream name once for the connection and **rejects the whole
//! resolution if any returned address is internal** — a name that straddles
//! public and internal addresses is the
//! shape of a DNS-rebinding attempt, so it fails closed rather than
//! cherry-picking a public address. Redirects are already disabled on the
//! upstream client, so there is no connect-time re-resolution to reopen the gap.
//!
//! The guard lives on the production upstream client only ([`crate::proxy::server::start_proxy`]);
//! tests that point the proxy at a loopback mock supply their own client through
//! `ProxyHandle::new`, so the mock is reachable without weakening the production
//! path.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// CGNAT shared address space, `100.64.0.0/10` (RFC 6598).
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (b & 0xc0) == 0x40
}

/// Whether an IPv4 address is in a baseline-deny (host-internal or otherwise
/// unsafe-to-egress) range. Covers the RFC-required set plus a few adjacent
/// special-use ranges, because this guard is the frozen base the egress track
/// (RFC-0002 "Egress profiles") builds on, so over-denying obscure non-public
/// space costs nothing.
fn is_internal_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_private()              // 10/8, 172.16/12, 192.168/16
        || ip.is_loopback()      // 127/8
        || ip.is_link_local()    // 169.254/16 (includes cloud metadata 169.254.169.254)
        || ip.is_broadcast()     // 255.255.255.255
        || ip.is_documentation() // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || ip.is_multicast()     // 224/4
        || is_cgnat(ip)          // 100.64/10
        || a == 0                // 0.0.0.0/8 "this network" (0.x reaches localhost on Linux)
        || (a == 198 && (b & 0xfe) == 18) // 198.18/15 benchmarking
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24 IETF protocol assignments
}

/// Whether an IPv6 address is in a baseline-deny range. IPv4-mapped
/// (`::ffff:a.b.c.d`) and the deprecated IPv4-compatible (`::a.b.c.d`) forms are
/// folded to their embedded IPv4 and classified there, so both
/// `::ffff:127.0.0.1` and `::127.0.0.1` are caught. 6to4 (`2002::/16`) and NAT64
/// (`64:ff9b::/96`) embeddings of an internal v4 are not decoded — theoretical
/// for the M0 fixed public upstreams, a hardening item for the egress track.
fn is_internal_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    if let Some(v4) = ip.to_ipv4() {
        return is_internal_v4(v4);
    }
    ip.is_multicast()                            // ff00::/8
        || (ip.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
        || (ip.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
}

/// Whether `ip` is in the baseline-deny set the proxy must never connect to on
/// the guest's behalf: RFC-1918, loopback, link-local/metadata, IPv6 ULA and
/// link-local, CGNAT, `0.0.0.0/8`, benchmarking/protocol-assignment ranges, and
/// unspecified/broadcast/multicast. The SLIRP gateway→host-loopback hop
/// (`10.0.2.x`, `127.0.0.1`) is already covered by the private and loopback
/// checks.
pub fn is_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_internal_v4(v4),
        IpAddr::V6(v6) => is_internal_v6(v6),
    }
}

/// A `reqwest` resolver that rejects any name resolving to an internal address.
#[derive(Debug, Default, Clone, Copy)]
pub struct SsrfGuardResolver;

impl Resolve for SsrfGuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            // Port 0: the connector overwrites it with the request's real port;
            // the internal-range check only inspects the IP.
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();

            if resolved.is_empty() {
                return Err(format!("SSRF guard: '{host}' did not resolve").into());
            }
            if let Some(internal) = resolved.iter().find(|addr| is_internal_ip(addr.ip())) {
                return Err(format!(
                    "SSRF guard: refusing upstream '{host}' — resolves to internal address {}",
                    internal.ip()
                )
                .into());
            }

            let addrs: Addrs = Box::new(resolved.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse().unwrap())
    }

    #[test]
    fn flags_internal_ipv4_ranges() {
        for ip in [
            "127.0.0.1",       // loopback
            "10.0.2.2",        // RFC-1918 (SLIRP gateway)
            "10.0.2.3",        // RFC-1918 (SLIRP DNS)
            "172.16.0.1",      // RFC-1918
            "192.168.1.1",     // RFC-1918
            "169.254.169.254", // link-local / cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",         // 0.0.0.0/8
            "0.1.2.3",         // 0.0.0.0/8 (reaches localhost on Linux)
            "255.255.255.255", // broadcast
            "198.18.0.1",      // benchmarking 198.18/15
            "198.19.255.255",  // benchmarking 198.18/15
            "192.0.0.8",       // IETF protocol assignments 192.0.0/24
            "192.0.2.1",       // documentation TEST-NET-1
        ] {
            assert!(is_internal_ip(v4(ip)), "{ip} should be internal");
        }
    }

    #[test]
    fn allows_public_ipv4() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "160.79.104.10", // anthropic
            "198.20.0.1",    // just outside 198.18/15
        ] {
            assert!(!is_internal_ip(v4(ip)), "{ip} should be public");
        }
    }

    #[test]
    fn flags_internal_ipv6_ranges() {
        for ip in [
            "::1",                    // loopback
            "::",                     // unspecified
            "fc00::1",                // unique-local
            "fd12:3456::1",           // unique-local
            "fe80::1",                // link-local
            "::ffff:127.0.0.1",       // IPv4-mapped loopback
            "::ffff:169.254.169.254", // IPv4-mapped metadata
            "::ffff:10.1.2.3",        // IPv4-mapped RFC-1918
            "::127.0.0.1",            // deprecated IPv4-compatible loopback
        ] {
            assert!(is_internal_ip(v6(ip)), "{ip} should be internal");
        }
    }

    #[test]
    fn allows_public_ipv6() {
        assert!(!is_internal_ip(v6("2606:4700:4700::1111"))); // Cloudflare
        assert!(!is_internal_ip(v6("2001:4860:4860::8888"))); // Google
        assert!(!is_internal_ip(v6("::ffff:8.8.8.8"))); // IPv4-mapped public
    }
}
