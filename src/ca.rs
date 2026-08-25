use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, Issuer, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::dns;

const CA_CERT_FILE: &str = "ca-cert.pem";
const CA_KEY_FILE: &str = "ca-key.pem";

/// A loaded (or freshly generated) local CA, kept in memory as both its
/// signing material (for minting leaf certs on demand) and its PEM (for
/// re-installing into the system trust store if ever needed again).
pub struct LoadedCa {
    key_pair: KeyPair,
    cert_pem: String,
    cert_der: CertificateDer<'static>,
}

impl LoadedCa {
    /// The CA's own certificate, DER-encoded — exposed only for tests
    /// outside this module (`proxy::tests`) that need to build a client
    /// `RootCertStore` trusting it.
    #[cfg(test)]
    pub fn cert_der_for_tests(&self) -> CertificateDer<'static> {
        self.cert_der.clone()
    }
}

/// Generates a fresh, unpersisted CA — exposed only for tests outside this
/// module that need one without touching the filesystem or system trust
/// store (see `ensure_ca` for the real, persisted/testable-separately path).
#[cfg(test)]
pub fn generate_ca_for_tests() -> LoadedCa {
    generate_ca().expect("CA generation must succeed in tests")
}

/// Loads the CA from `dir` (`ca-cert.pem` + `ca-key.pem`), generating and
/// persisting a new one if this is the first run. `dir` should be a durable
/// location (this project's convention is `/var/lib/fghjd/...`, *not*
/// `/var/run`: unlike the pidfile/port file, this CA must survive a reboot).
///
/// Deliberately does not touch the system trust store — that's
/// [`install_macos_trust`], kept as a separate step (mirroring
/// `dns::bind`/`dns::install_os_resolver_config`) so this function stays a
/// plain, testable filesystem operation with no privileged side effects.
pub fn ensure_ca(dir: &Path) -> Result<LoadedCa> {
    let cert_path = dir.join(CA_CERT_FILE);
    let key_path = dir.join(CA_KEY_FILE);

    if cert_path.exists() && key_path.exists() {
        return load_ca(&cert_path, &key_path);
    }

    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let ca = generate_ca()?;

    fs::write(&cert_path, &ca.cert_pem).with_context(|| format!("failed to write {}", cert_path.display()))?;
    let key_pem = ca.key_pair.serialize_pem();
    fs::write(&key_path, &key_pem).with_context(|| format!("failed to write {}", key_path.display()))?;
    // Root-only-readable: this key can mint a certificate for any hostname
    // that a browser trusting our CA will accept without warning.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on {}", key_path.display()))?;

    Ok(ca)
}

/// Path to the CA certificate PEM `ensure_ca` persists under `dir` — exposed
/// so callers (e.g. `daemon::run_control_api`) can pass it to
/// `install_macos_trust` without hardcoding the filename twice.
pub fn ca_cert_path(dir: &Path) -> PathBuf {
    dir.join(CA_CERT_FILE)
}

fn load_ca(cert_path: &Path, key_path: &Path) -> Result<LoadedCa> {
    let cert_pem =
        fs::read_to_string(cert_path).with_context(|| format!("failed to read {}", cert_path.display()))?;
    let key_pem =
        fs::read_to_string(key_path).with_context(|| format!("failed to read {}", key_path.display()))?;
    let key_pair = KeyPair::from_pem(&key_pem).context("failed to parse persisted CA private key")?;
    let cert_der = pem_to_der(&cert_pem).context("failed to parse persisted CA certificate")?;
    Ok(LoadedCa { key_pair, cert_pem, cert_der })
}

fn generate_ca() -> Result<LoadedCa> {
    let key_pair = KeyPair::generate().context("failed to generate CA key pair")?;
    let mut params = CertificateParams::new(Vec::new()).context("failed to construct CA cert params")?;
    params.distinguished_name.push(DnType::CommonName, "fghj local CA");
    params.distinguished_name.push(DnType::OrganizationName, "fghj");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let cert = params.self_signed(&key_pair).context("failed to self-sign CA certificate")?;
    let cert_pem = cert.pem();
    let cert_der = cert.der().clone();

    Ok(LoadedCa { key_pair, cert_pem, cert_der })
}

fn pem_to_der(pem: &str) -> Result<CertificateDer<'static>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::certs(&mut reader)
        .next()
        .context("PEM contains no certificate")?
        .context("failed to parse PEM certificate")
}

/// Installs `ca_cert_path` into the macOS System keychain as a trusted root,
/// so certificates this CA issues are accepted by the browser without a
/// warning. Requires root (matches the rest of `fghjd`'s privilege model).
/// Safe to call on every `fghjd` start, same as `dns::install_os_resolver_config`
/// — re-trusting an already-trusted cert is a harmless no-op, and `fghjd`
/// already runs fully as root so it never triggers an interactive prompt.
pub fn install_macos_trust(ca_cert_path: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        eprintln!(
            "fghjd: automatic system trust installation isn't implemented on this platform yet — \
             trust {} manually so browsers accept fghj's issued certificates",
            ca_cert_path.display()
        );
        return Ok(());
    }

    let status = Command::new("security")
        .args(["add-trusted-cert", "-d", "-r", "trustRoot", "-k", "/Library/Keychains/System.keychain"])
        .arg(ca_cert_path)
        .status()
        .context("failed to run `security add-trusted-cert`")?;
    if !status.success() {
        anyhow::bail!("`security add-trusted-cert` failed for {}", ca_cert_path.display());
    }
    println!("fghjd: installed the fghj local CA into the System trust store");
    Ok(())
}

/// Resolves a TLS certificate for any in-zone SNI on demand, minting and
/// caching a fresh leaf certificate signed by the loaded CA the first time
/// each hostname is seen. A single wildcard certificate can't cover this:
/// X.509 wildcards only match one leftmost label (RFC 6125), so
/// `*.fghj.internal` wouldn't match a multi-label name like
/// `deep.sub.fghj.internal` — and the schema allows exactly those.
#[derive(Debug)]
pub struct DynamicCertResolver {
    ca_key_pair: KeyPair,
    ca_cert_der: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl DynamicCertResolver {
    pub fn new(ca: LoadedCa, provider: Arc<CryptoProvider>) -> Self {
        Self { ca_key_pair: ca.key_pair, ca_cert_der: ca.cert_der, provider, cache: Mutex::new(HashMap::new()) }
    }

    fn issue(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        let issuer = Issuer::from_ca_cert_der(&self.ca_cert_der, &self.ca_key_pair)
            .context("failed to build issuer from CA certificate")?;

        let leaf_key = KeyPair::generate().context("failed to generate leaf key pair")?;
        let mut params =
            CertificateParams::new(vec![name.to_string()]).context("failed to construct leaf cert params")?;
        params.distinguished_name.push(DnType::CommonName, name);

        let cert = params.signed_by(&leaf_key, &issuer).context("failed to sign leaf certificate")?;

        let cert_chain = vec![cert.der().clone()];
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let certified = CertifiedKey::from_der(cert_chain, key_der, &self.provider)
            .context("failed to build rustls CertifiedKey from issued leaf certificate")?;
        Ok(Arc::new(certified))
    }

    /// The actual resolution logic, factored out of the `ResolvesServerCert`
    /// impl so it's callable directly from tests — rustls's `ClientHello` has
    /// no public constructor, so exercising `resolve()` itself would require
    /// driving a full handshake for what is otherwise a plain lookup.
    fn resolve_for(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        if !dns::in_zone(name) {
            return None;
        }

        if let Some(cached) = self.cache.lock().unwrap().get(name) {
            return Some(cached.clone());
        }

        let certified = self.issue(name).map_err(|e| eprintln!("fghjd: failed to issue cert for {name}: {e}")).ok()?;
        self.cache.lock().unwrap().insert(name.to_string(), certified.clone());
        Some(certified)
    }
}

impl ResolvesServerCert for DynamicCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.resolve_for(client_hello.server_name()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    #[test]
    fn generates_and_reloads_a_ca_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ca");

        let first = ensure_ca(&dir).unwrap();
        assert!(dir.join(CA_CERT_FILE).exists());
        assert!(dir.join(CA_KEY_FILE).exists());

        // Second call must load the same CA rather than regenerating it.
        let second = ensure_ca(&dir).unwrap();
        assert_eq!(first.cert_pem, second.cert_pem);
    }

    #[test]
    fn resolver_issues_cert_for_in_zone_name_and_rejects_out_of_zone() {
        let ca = generate_ca().unwrap();
        let resolver = DynamicCertResolver::new(ca, provider());

        assert!(resolver.resolve_for("cart.fghj.internal").is_some());
        assert!(resolver.resolve_for("evil.com").is_none());
    }

    #[test]
    fn resolver_caches_repeated_lookups_for_the_same_name() {
        let ca = generate_ca().unwrap();
        let resolver = DynamicCertResolver::new(ca, provider());

        let a = resolver.resolve_for("cart.fghj.internal").unwrap();
        let b = resolver.resolve_for("cart.fghj.internal").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second lookup should hit the cache, not mint a new cert");
    }

    #[test]
    fn issued_leaf_cert_chains_to_the_ca() {
        let ca = generate_ca().unwrap();
        let ca_cert_der = ca.cert_der.clone();
        let resolver = DynamicCertResolver::new(ca, provider());

        let certified = resolver.resolve_for("cart.fghj.internal").unwrap();
        let leaf_der = certified.cert[0].clone();

        // Cryptographically verify leaf_der was signed by the CA's key, not
        // just "resolve_for() didn't panic".
        use x509_parser::prelude::*;
        let (_, leaf) = X509Certificate::from_der(&leaf_der).unwrap();
        let (_, ca_cert) = X509Certificate::from_der(&ca_cert_der).unwrap();
        assert!(
            leaf.verify_signature(Some(ca_cert.public_key())).is_ok(),
            "leaf certificate signature must verify against the CA's public key"
        );
    }
}
