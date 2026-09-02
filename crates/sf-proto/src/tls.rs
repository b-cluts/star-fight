//! Transport trust: a pinned self-signed certificate (M4 security).
//!
//! The server mints one self-signed certificate and prints its SHA-256
//! fingerprint; players receive that fingerprint out-of-band and the client
//! accepts a TLS connection only if the presented certificate matches it.
//! No CA, no domain name, no renewal — and a man-in-the-middle would need a
//! certificate with a colliding SHA-256, which is computationally infeasible.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

/// Default server port.
pub const DEFAULT_PORT: u16 = 7777;
/// Shortest accepted fingerprint prefix (64 bits of hex).
pub const MIN_FINGERPRINT_HEX: usize = 16;

/// Lower-case hex SHA-256 of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der).iter().map(|b| format!("{b:02x}")).collect()
}

/// Tidy a pasted fingerprint: strip separators, lower-case, and require a
/// long enough hex prefix to pin on.
pub fn normalize_fingerprint(raw: &str) -> Result<String, String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':' && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if cleaned.len() < MIN_FINGERPRINT_HEX {
        return Err(format!("fingerprint must be at least {MIN_FINGERPRINT_HEX} hex characters"));
    }
    if cleaned.len() > 64 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("fingerprint must be hex (0-9, a-f), at most 64 characters".into());
    }
    Ok(cleaned)
}

/// Where and how to reach a server, as typed or pasted into the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Normalized fingerprint (prefix) to pin; `None` means plaintext
    /// `ws://` (development only — the server must run with --insecure).
    pub fingerprint: Option<String>,
}

impl Target {
    /// The copy-pasteable join string printed by the server.
    pub fn join_string(&self) -> String {
        match &self.fingerprint {
            Some(fp) => format!("starfight://{}:{}/#{fp}", self.host, self.port),
            None => format!("ws://{}:{}", self.host, self.port),
        }
    }

    /// `host:port` — the key under which a pin is remembered.
    pub fn key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Parse `starfight://host:port/#fingerprint`, `host:port`, `host`,
/// `ws://host:port` (plaintext) or `wss://host:port`. A separately typed
/// fingerprint (`extra`) fills in when the address carries none; when both
/// are given they must agree (one a prefix of the other).
pub fn parse_target(addr: &str, extra_fingerprint: &str) -> Result<Target, String> {
    let s = addr.trim();
    let (plain, rest) = if let Some(r) = s.strip_prefix("starfight://") {
        (false, r)
    } else if let Some(r) = s.strip_prefix("wss://") {
        (false, r)
    } else if let Some(r) = s.strip_prefix("ws://") {
        (true, r)
    } else {
        (false, s)
    };
    let (hostport, frag) = match rest.split_once('#') {
        Some((h, f)) => (h, Some(f)),
        None => (rest, None),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h, p.parse::<u16>().map_err(|_| format!("bad port {p}"))?)
        }
        _ => (hostport, DEFAULT_PORT),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err("enter the server address".into());
    }
    let from_addr = frag.map(normalize_fingerprint).transpose()?;
    let extra = extra_fingerprint.trim();
    let typed = if extra.is_empty() { None } else { Some(normalize_fingerprint(extra)?) };
    let fingerprint = match (from_addr, typed) {
        (Some(a), Some(b)) => {
            if a.starts_with(&b) {
                Some(a)
            } else if b.starts_with(&a) {
                Some(b)
            } else {
                return Err("the join string and the typed fingerprint disagree".into());
            }
        }
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    if plain && fingerprint.is_some() {
        return Err("ws:// is plaintext; use starfight:// with a fingerprint".into());
    }
    if !plain && fingerprint.is_none() {
        return Err(
            "enter the server's certificate fingerprint (or paste its starfight:// join string)"
                .into(),
        );
    }
    Ok(Target { host: host.to_string(), port, fingerprint })
}

/// Certificate pinning verifier: accepts exactly the certificate whose
/// SHA-256 fingerprint starts with the pinned prefix.
#[derive(Debug)]
pub struct PinnedCert {
    prefix: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let hex = fingerprint(end_entity.as_ref());
        if hex.starts_with(&self.prefix) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch (server presented {}…)",
                &hex[..MIN_FINGERPRINT_HEX]
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A rustls client config that trusts only the pinned certificate.
/// `prefix` must already be normalized (see [`normalize_fingerprint`]).
pub fn pinned_client_config(prefix: String) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { prefix, provider }))
        .with_no_client_auth();
    Arc::new(config)
}

/// A `ServerName` for the handshake: pinning ignores it, but rustls needs
/// one. Any host string works (IP addresses included).
pub fn server_name(host: &str) -> ServerName<'static> {
    ServerName::try_from(host.to_string())
        .unwrap_or_else(|_| ServerName::try_from("starfight").expect("static name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_lowercase_hex_sha256() {
        let fp = fingerprint(b"hello");
        assert_eq!(fp.len(), 64);
        assert!(fp.starts_with("2cf24dba5fb0a30e"));
    }

    #[test]
    fn normalize_strips_separators_and_enforces_length() {
        assert_eq!(
            normalize_fingerprint("AB:CD ef-01 2345 6789 abcd").unwrap(),
            "abcdef0123456789abcd"
        );
        assert!(normalize_fingerprint("abcdef").is_err());
        assert!(normalize_fingerprint("zzzzzzzzzzzzzzzz").is_err());
    }

    #[test]
    fn parse_join_string_and_variants() {
        let t = parse_target("starfight://10.0.0.5:7777/#ABCDEF0123456789aa", "").unwrap();
        assert_eq!(t.host, "10.0.0.5");
        assert_eq!(t.port, 7777);
        assert_eq!(t.fingerprint.as_deref(), Some("abcdef0123456789aa"));
        assert_eq!(t.join_string(), "starfight://10.0.0.5:7777/#abcdef0123456789aa");
        assert_eq!(t.key(), "10.0.0.5:7777");

        let t = parse_target("example.org", "abcdef0123456789").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("example.org", DEFAULT_PORT));
        assert_eq!(t.fingerprint.as_deref(), Some("abcdef0123456789"));

        let t = parse_target("ws://127.0.0.1:9000", "").unwrap();
        assert_eq!(t.port, 9000);
        assert!(t.fingerprint.is_none());
        assert_eq!(t.join_string(), "ws://127.0.0.1:9000");

        // Typed prefix of the join string's fingerprint: the longer one wins.
        let t = parse_target("starfight://h:1/#abcdef0123456789aabb", "abcdef0123456789").unwrap();
        assert_eq!(t.fingerprint.as_deref(), Some("abcdef0123456789aabb"));
    }

    #[test]
    fn parse_rejects_missing_or_conflicting_pins() {
        assert!(parse_target("example.org", "").is_err());
        assert!(parse_target("", "abcdef0123456789").is_err());
        assert!(parse_target("starfight://h:1/#abcdef0123456789", "0123456789abcdef").is_err());
        assert!(parse_target("ws://h:1", "abcdef0123456789").is_err());
    }

    #[test]
    fn pinned_verifier_accepts_only_matching_prefix() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert = CertificateDer::from(b"fake-der".to_vec());
        let fp = fingerprint(cert.as_ref());
        let ok = PinnedCert { prefix: fp[..20].to_string(), provider: provider.clone() };
        let bad = PinnedCert { prefix: "0000000000000000".into(), provider };
        let name = server_name("127.0.0.1");
        assert!(ok.verify_server_cert(&cert, &[], &name, &[], UnixTime::now()).is_ok());
        assert!(bad.verify_server_cert(&cert, &[], &name, &[], UnixTime::now()).is_err());
    }
}
