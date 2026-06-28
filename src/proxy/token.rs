//! Per-sandbox proxy authentication token.
//!
//! The token is a neighbour guard, not an in-guest-adversary control: it is
//! readable by the guest (it has to be, so the configured client can present
//! it), so its job is to stop one sandbox from using another sandbox's proxy
//! sandbox-slot, and to give the proxy a stable key to resolve the per-sandbox
//! [`SandboxContext`](crate::proxy::SandboxContext). It is checked on every guest
//! connection and **stripped** before the request is forwarded upstream so it
//! never leaks to the provider.
//!
//! It is deliberately *not* a defence against a uid-1000 adversary that already
//! controls the guest — that adversary cannot obtain a usable credential anyway,
//! because the durable secret never enters the guest (the proxy injects it).

use std::fmt;

use subtle::ConstantTimeEq;

/// Number of random bytes behind a token. 256 bits of entropy makes the token
/// space large enough that guessing another sandbox's slot is infeasible.
const TOKEN_BYTES: usize = 32;

/// HTTP header the guest client uses to present its per-sandbox proxy token.
///
/// Carried on the guest→proxy hop only; the proxy strips it before
/// re-originating to the upstream.
pub const PROXY_TOKEN_HEADER: &str = "x-voidbox-proxy-token";

/// A per-sandbox proxy token. Compares in constant time and redacts itself in
/// `Debug` so it never lands in logs verbatim.
#[derive(Clone)]
pub struct ProxyToken {
    bytes: [u8; TOKEN_BYTES],
}

impl ProxyToken {
    /// Generate a fresh random token from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).expect("OS CSPRNG must be available");
        Self { bytes }
    }

    /// Render the token as lowercase hex for transport in an env var / header.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(TOKEN_BYTES * 2);
        for byte in self.bytes {
            out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
            out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
        }
        out
    }

    /// Parse a hex-encoded token presented by a guest client.
    ///
    /// Returns `None` on any malformed input rather than partially decoding, so
    /// callers treat an unparseable token exactly like a wrong one.
    pub fn from_hex(input: &str) -> Option<Self> {
        if input.len() != TOKEN_BYTES * 2 {
            return None;
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let hi = input.as_bytes()[index * 2];
            let lo = input.as_bytes()[index * 2 + 1];
            let hi = (hi as char).to_digit(16)?;
            let lo = (lo as char).to_digit(16)?;
            *slot = ((hi << 4) | lo) as u8;
        }
        Some(Self { bytes })
    }

    /// Constant-time equality check against a token presented by a client.
    pub fn matches(&self, presented: &ProxyToken) -> bool {
        self.bytes.ct_eq(&presented.bytes).into()
    }
}

impl fmt::Debug for ProxyToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProxyToken(redacted)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let token = ProxyToken::generate();
        let restored = ProxyToken::from_hex(&token.to_hex()).expect("valid hex");
        assert!(token.matches(&restored));
    }

    #[test]
    fn distinct_tokens_do_not_match() {
        let a = ProxyToken::generate();
        let b = ProxyToken::generate();
        assert!(!a.matches(&b));
    }

    #[test]
    fn rejects_malformed_hex() {
        assert!(ProxyToken::from_hex("nothex").is_none());
        assert!(ProxyToken::from_hex("").is_none());
        // Right length, non-hex characters.
        assert!(ProxyToken::from_hex(&"z".repeat(TOKEN_BYTES * 2)).is_none());
        // Hex but wrong length.
        assert!(ProxyToken::from_hex("abcd").is_none());
    }

    #[test]
    fn debug_is_redacted() {
        let token = ProxyToken::generate();
        assert_eq!(format!("{token:?}"), "ProxyToken(redacted)");
    }
}
