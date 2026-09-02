//! Server TLS identity: a self-signed certificate minted on first run and
//! persisted so the fingerprint players have pinned stays stable.

use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub use sf_proto::tls::fingerprint;

pub const CERT_FILE: &str = "tls_cert.pem";
pub const KEY_FILE: &str = "tls_key.pem";

/// Load `tls_cert.pem` / `tls_key.pem` from `dir`, or generate and write
/// them when absent.
pub fn load_or_create_identity(
    dir: &Path,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), String> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    if cert_path.exists() && key_path.exists() {
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(
            fs::File::open(&cert_path).map_err(|e| format!("read {}: {e}", cert_path.display()))?,
        ))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("parse {}: {e}", cert_path.display()))?;
        let key = rustls_pemfile::private_key(&mut BufReader::new(
            fs::File::open(&key_path).map_err(|e| format!("read {}: {e}", key_path.display()))?,
        ))
        .map_err(|e| format!("parse {}: {e}", key_path.display()))?
        .ok_or_else(|| format!("no private key in {}", key_path.display()))?;
        let cert = certs
            .into_iter()
            .next()
            .ok_or_else(|| format!("no certificate in {}", cert_path.display()))?;
        return Ok((cert, key));
    }

    let ck = rcgen::generate_simple_self_signed(vec!["starfight".to_string()])
        .map_err(|e| format!("generate certificate: {e}"))?;
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    fs::write(&cert_path, ck.cert.pem())
        .map_err(|e| format!("write {}: {e}", cert_path.display()))?;
    fs::write(&key_path, ck.key_pair.serialize_pem())
        .map_err(|e| format!("write {}: {e}", key_path.display()))?;
    let cert = ck.cert.der().clone();
    let key = PrivateKeyDer::try_from(ck.key_pair.serialize_der())
        .map_err(|e| format!("key der: {e}"))?;
    Ok((cert, key))
}

/// A rustls server config presenting exactly this identity.
pub fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| format!("server certificate: {e}"))?;
    Ok(Arc::new(config))
}
