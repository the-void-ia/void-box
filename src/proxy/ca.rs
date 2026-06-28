//! Per-run, name-constrained certificate authority for the injection proxy.
//!
//! The proxy terminates the guest's TLS so it can rewrite the credential
//! header. To make that trustable without turning the guest into a universal
//! MITM target, each run gets its own short-lived CA whose authority is
//! **name-constrained** to exactly the upstream hosts the run injects into. A
//! leaked or guest-readable CA cert can therefore impersonate only those
//! upstreams, not arbitrary sites — and only the public cert is ever installed
//! in the guest; the private key never leaves this host process (so it is
//! structurally absent from any snapshot).
//!
//! Leaf certificates are minted lazily, per TLS ClientHello SNI, by
//! [`LeafResolver`] and cached for the run's lifetime, so a single per-run
//! listener can serve several credentialed upstreams (Phase 0 uses one).
//!
//! ECDSA P-256 is chosen because keygen is on the cold-start budget and P-256
//! generation is cheap relative to RSA.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, KeyPair, KeyUsagePurpose, NameConstraints, PKCS_ECDSA_P256_SHA256,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;

use crate::error::{Error, Result};

/// A per-run CA that mints leaf certificates name-constrained to the run's
/// injected upstream hosts.
pub struct ProxyCa {
    /// PEM of the CA's public certificate — the only CA material that enters
    /// the guest (via the additive trust env path).
    ca_cert_pem: String,
    /// The CA certificate, used as the issuer when signing per-host leaves.
    ca_cert: Certificate,
    /// The CA private key. Stays in this host process only — never the guest,
    /// the cmdline, or a snapshot.
    ca_key: KeyPair,
    /// Hosts this CA is name-constrained to, mirrored for leaf-SAN validation.
    allowed_upstreams: Vec<String>,
}

impl ProxyCa {
    /// Generate a fresh per-run CA name-constrained to `allowed_upstreams`.
    pub fn generate(allowed_upstreams: Vec<String>) -> Result<Self> {
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map_err(|e| Error::Network(format!("proxy CA keygen failed: {e}")))?;

        let mut params = CertificateParams::new(Vec::new())
            .map_err(|e| Error::Network(format!("proxy CA params failed: {e}")))?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params
            .distinguished_name
            .push(DnType::CommonName, "void-box per-run proxy CA");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: allowed_upstreams
                .iter()
                .map(|host| GeneralSubtree::DnsName(host.clone()))
                .collect(),
            excluded_subtrees: Vec::new(),
        });

        let ca_cert = params
            .self_signed(&ca_key)
            .map_err(|e| Error::Network(format!("proxy CA self-sign failed: {e}")))?;
        let ca_cert_pem = ca_cert.pem();

        Ok(Self {
            ca_cert_pem,
            ca_cert,
            ca_key,
            allowed_upstreams,
        })
    }

    /// The CA's public certificate in PEM form — install this in the guest.
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// Whether `host` is within this CA's name constraints.
    pub fn permits_host(&self, host: &str) -> bool {
        self.allowed_upstreams
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    }

    /// Mint a leaf certificate + signing key for `host`, which must be within
    /// the CA's name constraints.
    pub fn mint_certified_key(&self, host: &str) -> Result<Arc<CertifiedKey>> {
        if !self.permits_host(host) {
            return Err(Error::Network(format!(
                "proxy CA refuses to mint a leaf for out-of-constraint host: {host}"
            )));
        }

        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map_err(|e| Error::Network(format!("proxy leaf keygen failed: {e}")))?;
        let mut leaf_params = CertificateParams::new(vec![host.to_string()])
            .map_err(|e| Error::Network(format!("proxy leaf params failed: {e}")))?;
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, host);
        leaf_params.use_authority_key_identifier_extension = true;
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &self.ca_cert, &self.ca_key)
            .map_err(|e| Error::Network(format!("proxy leaf sign failed: {e}")))?;

        let chain: Vec<CertificateDer<'static>> = vec![leaf_cert.der().clone()];
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let signing_key = any_ecdsa_type(&key_der)
            .map_err(|e| Error::Network(format!("proxy leaf signing key failed: {e}")))?;

        Ok(Arc::new(CertifiedKey::new(chain, signing_key)))
    }

    /// Build a rustls [`ServerConfig`] that mints/caches per-SNI leaves from
    /// this CA. SNIs outside the name constraints fail the handshake.
    ///
    /// Leaves for the allowed upstreams are **pre-minted** here so the keygen +
    /// signature cost is paid at config-build time, not on the latency-sensitive
    /// first TLS handshake (which then always hits the cache).
    pub fn server_config(self: &Arc<Self>) -> Arc<ServerConfig> {
        let cache: HashMap<String, Arc<CertifiedKey>> = self
            .allowed_upstreams
            .iter()
            .filter_map(|host| Some((host.clone(), self.mint_certified_key(host).ok()?)))
            .collect();
        let resolver = Arc::new(LeafResolver {
            ca: self.clone(),
            cache: Mutex::new(cache),
        });
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring provider supports the default protocol versions")
                .with_no_client_auth()
                .with_cert_resolver(resolver);
        Arc::new(config)
    }
}

/// Resolves (and caches) a leaf certificate per requested SNI, refusing any
/// host outside the CA's name constraints.
#[derive(Debug)]
struct LeafResolver {
    ca: Arc<ProxyCa>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for ProxyCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyCa")
            .field("allowed_upstreams", &self.allowed_upstreams)
            .finish_non_exhaustive()
    }
}

impl ResolvesServerCert for LeafResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        // This runs per ClientHello on the guest-controlled parser surface, so
        // it must not panic: recover a poisoned lock rather than unwrap.
        let host = client_hello.server_name()?.to_owned();
        if let Some(existing) = self
            .cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&host)
        {
            return Some(existing.clone());
        }
        let minted = self.ca.mint_certified_key(&host).ok()?;
        self.cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(host, minted.clone());
        Some(minted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_pem_is_emitted() {
        let ca = ProxyCa::generate(vec!["api.anthropic.com".into()]).expect("generate CA");
        let pem = ca.ca_cert_pem();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(pem.trim_end().ends_with("-----END CERTIFICATE-----"));
    }

    #[test]
    fn mints_leaf_for_permitted_host() {
        let ca = ProxyCa::generate(vec!["api.anthropic.com".into()]).expect("generate CA");
        assert!(ca.permits_host("api.anthropic.com"));
        assert!(ca.permits_host("API.ANTHROPIC.COM"));
        assert!(ca.mint_certified_key("api.anthropic.com").is_ok());
    }

    #[test]
    fn refuses_leaf_for_out_of_constraint_host() {
        let ca = ProxyCa::generate(vec!["api.anthropic.com".into()]).expect("generate CA");
        assert!(!ca.permits_host("evil.example.com"));
        assert!(ca.mint_certified_key("evil.example.com").is_err());
    }

    #[test]
    fn server_config_builds() {
        let ca = Arc::new(ProxyCa::generate(vec!["api.anthropic.com".into()]).expect("CA"));
        let _config = ca.server_config();
    }
}
