use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::{dns, proxy};

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

    fs::write(&cert_path, &ca.cert_pem)
        .with_context(|| format!("failed to write {}", cert_path.display()))?;
    let key_pem = ca.key_pair.serialize_pem();
    fs::write(&key_path, &key_pem)
        .with_context(|| format!("failed to write {}", key_path.display()))?;
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

/// Path to the CA private key PEM `ensure_ca` persists under `dir` —
/// exposed so callers (e.g. the sidecar container's bind-mount setup in
/// `runs.rs`) can name the key file without reaching into `ca.rs` internals.
pub fn ca_key_path(dir: &Path) -> PathBuf {
    dir.join(CA_KEY_FILE)
}

const TRUST_CERT_FILE: &str = "cert.pem";
const TRUST_BUNDLE_FILE: &str = "bundle.pem";

/// Writes two world-readable, key-free files into `dir`, alongside the CA's
/// own (root-only-readable-key) material: `cert.pem` (just fghj's CA cert)
/// and `bundle.pem` (that same cert merged with this host's real root CA
/// store — a drop-in replacement for a container's own system trust file).
/// This is the entire feature surface for trusting fghj's zone from a
/// container: no `.fghj.yaml` schema of its own, just two stable paths a
/// workspace mounts itself via the existing generic `volumes:` mechanism.
///
/// Deliberately a separate step from `ensure_ca`, not folded into it: the
/// sidecar's own `ensure_ca` call (`fghj-sidecar.rs`) runs against a `:ro`
/// bind mount of this same directory, and would fail if `ensure_ca` itself
/// tried to write these back into it. Callers that own a writable `dir`
/// (currently just `daemon::run_control_api`) call this explicitly instead.
pub fn refresh_trust_files(dir: &Path, ca: &LoadedCa) -> Result<()> {
    write_world_readable(&dir.join(TRUST_CERT_FILE), ca.cert_pem.as_bytes())?;

    let mut bundle = String::new();
    for der in native_root_certs() {
        let pem = pem::Pem::new("CERTIFICATE", der.to_vec());
        bundle.push_str(&pem::encode_config(
            &pem,
            pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF),
        ));
    }
    bundle.push_str(&ca.cert_pem);
    write_world_readable(&dir.join(TRUST_BUNDLE_FILE), bundle.as_bytes())?;

    Ok(())
}

/// This host's real root CA store (macOS Keychain / Linux system bundle /
/// ...). Best-effort by design: `rustls-native-certs` documents that a
/// handful of unparsable OS entries is normal, and even a wholly empty
/// result (e.g. a minimal container with no system store at all) should
/// still leave `bundle.pem` usable — just equivalent to `cert.pem` alone —
/// rather than failing the whole refresh.
fn native_root_certs() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

fn write_world_readable(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    Ok(())
}

fn load_ca(cert_path: &Path, key_path: &Path) -> Result<LoadedCa> {
    let cert_pem = fs::read_to_string(cert_path)
        .with_context(|| format!("failed to read {}", cert_path.display()))?;
    let key_pem = fs::read_to_string(key_path)
        .with_context(|| format!("failed to read {}", key_path.display()))?;
    let key_pair =
        KeyPair::from_pem(&key_pem).context("failed to parse persisted CA private key")?;
    let cert_der = pem_to_der(&cert_pem).context("failed to parse persisted CA certificate")?;
    Ok(LoadedCa {
        key_pair,
        cert_pem,
        cert_der,
    })
}

fn generate_ca() -> Result<LoadedCa> {
    let key_pair = KeyPair::generate().context("failed to generate CA key pair")?;
    let mut params =
        CertificateParams::new(Vec::new()).context("failed to construct CA cert params")?;
    params
        .distinguished_name
        .push(DnType::CommonName, "fghj local CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "fghj");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let cert = params
        .self_signed(&key_pair)
        .context("failed to self-sign CA certificate")?;
    let cert_pem = cert.pem();
    let cert_der = cert.der().clone();

    Ok(LoadedCa {
        key_pair,
        cert_pem,
        cert_der,
    })
}

fn pem_to_der(pem: &str) -> Result<CertificateDer<'static>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::certs(&mut reader)
        .next()
        .context("PEM contains no certificate")?
        .context("failed to parse PEM certificate")
}

/// Whether `ca_cert_path` is already trusted as a root in the macOS System
/// keychain. `security verify-cert` is a trust *evaluation*, not a trust
/// *modification* — unlike `add-trusted-cert` it never triggers an
/// interactive Authorization Services prompt, so this is safe (and cheap) to
/// call unconditionally, including from a non-macOS caller that will never
/// invoke it.
fn is_trusted_on_macos(ca_cert_path: &Path) -> bool {
    Command::new("security")
        .args(["verify-cert", "-c"])
        .arg(ca_cert_path)
        .args(["-k", "/Library/Keychains/System.keychain"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ensures `ca_cert_path` is trusted as a root in the macOS System keychain,
/// so certificates this CA issues are accepted by the browser without a
/// warning. Safe to call on every `fghjd` start, same as
/// `dns::install_os_resolver_config`: it first checks whether the cert is
/// already trusted (a promptless read) and only falls through to the actual
/// `add-trusted-cert` write — which always triggers an interactive
/// Authorization Services password prompt on macOS, root privilege
/// notwithstanding — when it genuinely isn't. This also means trust that
/// goes missing after the fact (e.g. a user manually revokes it in Keychain
/// Access, or the keychain gets reset) self-heals on the next `fghjd`
/// restart instead of silently staying broken.
pub fn install_macos_trust(ca_cert_path: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        eprintln!(
            "fghjd: automatic system trust installation isn't implemented on this platform yet — \
             trust {} manually so browsers accept fghj's issued certificates",
            ca_cert_path.display()
        );
        return Ok(());
    }

    if is_trusted_on_macos(ca_cert_path) {
        return Ok(());
    }

    let status = Command::new("security")
        .args([
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            "/Library/Keychains/System.keychain",
        ])
        .arg(ca_cert_path)
        .status()
        .context("failed to run `security add-trusted-cert`")?;
    if !status.success() {
        anyhow::bail!(
            "`security add-trusted-cert` failed for {}",
            ca_cert_path.display()
        );
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
///
/// Also the eligibility gate for an `#AdditionalHost` alias: `routes` (the
/// same `RouteResolver` `proxy::serve_https` dispatches through) is
/// consulted so a reserved-TLD alias (`dns::is_reserved_alias`) only ever
/// gets a certificate while some running container actually claims it as a
/// route — never a blanket "any `.local`-shaped SNI gets a cert," which
/// would let a browser mint trust for a name nothing in this workspace
/// declared.
pub struct DynamicCertResolver {
    ca_key_pair: KeyPair,
    ca_cert_der: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
    routes: Arc<dyn proxy::RouteResolver>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for DynamicCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicCertResolver")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

impl DynamicCertResolver {
    pub fn new(
        ca: LoadedCa,
        provider: Arc<CryptoProvider>,
        routes: Arc<dyn proxy::RouteResolver>,
    ) -> Self {
        Self {
            ca_key_pair: ca.key_pair,
            ca_cert_der: ca.cert_der,
            provider,
            routes,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn issue(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        let issuer = Issuer::from_ca_cert_der(&self.ca_cert_der, &self.ca_key_pair)
            .context("failed to build issuer from CA certificate")?;

        let leaf_key = KeyPair::generate().context("failed to generate leaf key pair")?;
        let mut params = CertificateParams::new(vec![name.to_string()])
            .context("failed to construct leaf cert params")?;
        params.distinguished_name.push(DnType::CommonName, name);
        // RFC 5280 requires any non-self-signed certificate to carry an
        // AuthorityKeyIdentifier pointing back to its issuer's key.
        // `use_authority_key_identifier_extension` defaults to `false` in
        // rcgen, and it silently produced leaf certs with *no* AKI (and, via
        // `is_ca` defaulting to `NoCa`, no SubjectKeyIdentifier or
        // basicConstraints either — rcgen only writes those for certs whose
        // `is_ca` isn't left at its default). Newer OpenSSL enforces the AKI
        // requirement strictly and rejects the chain; looser stacks
        // (`openssl s_client`, browsers) don't, which is how this went
        // unnoticed. `ExplicitNoCa` marks this leaf as an explicit
        // (non-CA) end-entity cert, which is what actually turns on the SKI
        // and `basicConstraints: CA:FALSE` extensions.
        params.is_ca = IsCa::ExplicitNoCa;
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        let cert = params
            .signed_by(&leaf_key, &issuer)
            .context("failed to sign leaf certificate")?;

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
        let eligible = dns::cert_eligible(name, self.routes.resolve(name).is_some());
        if !eligible {
            return None;
        }

        if let Some(cached) = self.cache.lock().unwrap().get(name) {
            return Some(cached.clone());
        }

        let certified = self
            .issue(name)
            .map_err(|e| eprintln!("fghjd: failed to issue cert for {name}: {e}"))
            .ok()?;
        self.cache
            .lock()
            .unwrap()
            .insert(name.to_string(), certified.clone());
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

    /// A `RouteResolver` backed by a plain in-memory map — same role as
    /// `proxy::tests::StaticRoutes`, duplicated here (rather than exposed
    /// from `proxy`) since it's only ever needed to exercise the
    /// reserved-alias eligibility gate in isolation.
    struct StaticRoutes(std::collections::HashMap<&'static str, u16>);

    impl proxy::RouteResolver for StaticRoutes {
        fn resolve(&self, host: &str) -> Option<proxy::Backend> {
            self.0.get(host).copied().map(|port| proxy::Backend {
                host: "127.0.0.1".to_string(),
                port,
            })
        }
    }

    fn no_routes() -> Arc<dyn proxy::RouteResolver> {
        Arc::new(StaticRoutes(std::collections::HashMap::new()))
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
    fn refresh_trust_files_writes_a_bare_cert_and_a_merged_bundle_with_no_key_material() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = generate_ca_for_tests();

        refresh_trust_files(tmp.path(), &ca).unwrap();

        let cert = fs::read_to_string(tmp.path().join(TRUST_CERT_FILE)).unwrap();
        assert_eq!(cert, ca.cert_pem);
        assert!(
            !cert.contains("PRIVATE KEY"),
            "cert.pem must never contain key material"
        );

        let bundle = fs::read_to_string(tmp.path().join(TRUST_BUNDLE_FILE)).unwrap();
        assert!(
            bundle.ends_with(&ca.cert_pem),
            "fghj's own CA cert must be present (and last) in the merged bundle"
        );
        assert!(
            !bundle.contains("PRIVATE KEY"),
            "bundle.pem must never contain key material"
        );

        for path in [TRUST_CERT_FILE, TRUST_BUNDLE_FILE] {
            let mode = fs::metadata(tmp.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o644, "{path} must be world-readable");
        }
    }

    #[test]
    fn resolver_issues_cert_for_in_zone_name_and_rejects_out_of_zone() {
        let ca = generate_ca().unwrap();
        let resolver = DynamicCertResolver::new(ca, provider(), no_routes());

        assert!(resolver.resolve_for("cart.fghj.internal").is_some());
        assert!(resolver.resolve_for("evil.com").is_none());
    }

    #[test]
    fn resolver_issues_cert_for_a_routed_reserved_alias_but_not_an_unrouted_one() {
        let ca = generate_ca().unwrap();
        let routes: Arc<dyn proxy::RouteResolver> = Arc::new(StaticRoutes(
            std::collections::HashMap::from([("aikido.local", 8080)]),
        ));
        let resolver = DynamicCertResolver::new(ca, provider(), routes);

        assert!(
            resolver.resolve_for("aikido.local").is_some(),
            "a reserved-TLD alias that's actually routed must get a cert"
        );
        assert!(
            resolver.resolve_for("other.local").is_none(),
            "a reserved-TLD-shaped name nothing declared/routed must never get a cert"
        );
    }

    #[test]
    fn resolver_never_issues_a_cert_for_a_non_reserved_alias_even_if_routed() {
        let ca = generate_ca().unwrap();
        let routes: Arc<dyn proxy::RouteResolver> = Arc::new(StaticRoutes(
            std::collections::HashMap::from([("demo.example.com", 8080)]),
        ));
        let resolver = DynamicCertResolver::new(ca, provider(), routes);

        assert!(
            resolver.resolve_for("demo.example.com").is_none(),
            "a real, non-reserved-TLD hostname must never get a cert from fghj's local CA, routed or not"
        );
    }

    #[test]
    fn resolver_caches_repeated_lookups_for_the_same_name() {
        let ca = generate_ca().unwrap();
        let resolver = DynamicCertResolver::new(ca, provider(), no_routes());

        let a = resolver.resolve_for("cart.fghj.internal").unwrap();
        let b = resolver.resolve_for("cart.fghj.internal").unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "second lookup should hit the cache, not mint a new cert"
        );
    }

    #[test]
    fn issued_leaf_cert_chains_to_the_ca() {
        let ca = generate_ca().unwrap();
        let ca_cert_der = ca.cert_der.clone();
        let resolver = DynamicCertResolver::new(ca, provider(), no_routes());

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

    /// RFC 5280 requires a non-self-signed certificate to carry an
    /// AuthorityKeyIdentifier pointing back to its issuer's key; strict
    /// verifiers (newer OpenSSL, unlike `openssl s_client` or browsers)
    /// reject a chain without one. Also checks for the SubjectKeyIdentifier
    /// and `basicConstraints: CA:FALSE` extensions that come along with
    /// marking the leaf `IsCa::ExplicitNoCa` rather than leaving it at
    /// rcgen's default `NoCa`.
    #[test]
    fn issued_leaf_cert_carries_aki_ski_and_basic_constraints() {
        let ca = generate_ca().unwrap();
        let ca_cert_der = ca.cert_der.clone();
        let resolver = DynamicCertResolver::new(ca, provider(), no_routes());

        let certified = resolver.resolve_for("cart.fghj.internal").unwrap();
        let leaf_der = certified.cert[0].clone();

        use x509_parser::extensions::ParsedExtension;
        use x509_parser::prelude::*;
        let (_, leaf) = X509Certificate::from_der(&leaf_der).unwrap();
        let (_, ca_cert) = X509Certificate::from_der(&ca_cert_der).unwrap();

        let find_ski = |cert: &X509Certificate| -> Vec<u8> {
            cert.iter_extensions()
                .find_map(|ext| match ext.parsed_extension() {
                    ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
                    _ => None,
                })
                .expect("cert must carry a SubjectKeyIdentifier")
        };
        let ca_ski = find_ski(&ca_cert);
        let leaf_aki = leaf
            .iter_extensions()
            .find_map(|ext| match ext.parsed_extension() {
                ParsedExtension::AuthorityKeyIdentifier(aki) => {
                    aki.key_identifier.as_ref().map(|id| id.0.to_vec())
                }
                _ => None,
            })
            .expect("leaf cert must carry an AuthorityKeyIdentifier");
        assert_eq!(
            leaf_aki, ca_ski,
            "leaf's AuthorityKeyIdentifier must match the CA's SubjectKeyIdentifier"
        );
        find_ski(&leaf); // asserts (via find_ski's own expect) that the leaf has its own SKI too

        let bc = leaf
            .basic_constraints()
            .unwrap()
            .expect("leaf cert must carry a basicConstraints extension")
            .value;
        assert!(!bc.ca, "leaf cert's basicConstraints must be CA:FALSE");
    }
}
