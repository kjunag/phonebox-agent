use std::fs;
use std::path::Path;

use openssl::pkey::PKey;
use openssl::ssl::{SslAcceptor, SslMethod};
use openssl::x509::X509;
use sha2::{Digest, Sha256};

pub struct Tls {
    pub acceptor: SslAcceptor,
    pub fingerprint: String,
}

pub fn load(path: &Path) -> Result<Tls, String> {
    let der = fs::read(path).map_err(|err| err.to_string())?;
    let p12 = openssl::pkcs12::Pkcs12::from_der(&der).map_err(|err| err.to_string())?;
    let parsed = p12.parse2("").map_err(|err| err.to_string())?;
    let cert = parsed.cert.ok_or_else(|| "certificate missing".to_string())?;
    let key = parsed.pkey.ok_or_else(|| "private key missing".to_string())?;
    let fingerprint = hex_sha256(&cert.to_der().map_err(|err| err.to_string())?);
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).map_err(|err| err.to_string())?;
    builder.set_private_key(&key).map_err(|err| err.to_string())?;
    builder.set_certificate(&cert).map_err(|err| err.to_string())?;
    if let Some(chain) = parsed.ca {
        for extra in chain {
            builder.add_extra_chain_cert(extra).map_err(|err| err.to_string())?;
        }
    }
    builder.check_private_key().map_err(|err| err.to_string())?;
    Ok(Tls { acceptor: builder.build(), fingerprint })
}

pub fn fingerprint(path: &Path) -> Option<String> {
    let der = fs::read(path).ok()?;
    let p12 = openssl::pkcs12::Pkcs12::from_der(&der).ok()?;
    let parsed = p12.parse2("").ok()?;
    let cert: X509 = parsed.cert?;
    Some(hex_sha256(&cert.to_der().ok()?))
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[allow(dead_code)]
fn _key_type(key: &PKey<openssl::pkey::Private>) -> &'static str {
    if key.ec_key().is_ok() { "ec" } else { "other" }
}
