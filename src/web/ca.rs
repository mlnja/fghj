//! The local CA behind every `https://*.fghj.internal` URL: loading it,
//! minting leaf certs on demand, and publishing PEMs for containers. Which
//! names get a cert: see `DynamicCertResolver::resolve_for`.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::dns;
use crate::web::proxy;

const CA_CERT_FILE: &str = "ca-cert.pem";
const CA_KEY_FILE: &str = "ca-key.pem";

pub struct LoadedCa {
    key_pair: KeyPair,
    cert_pem: String,
    cert_der: CertificateDer<'static>,
}

impl LoadedCa {
    #[cfg(test)]
    pub fn cert_der_for_tests(&self) -> CertificateDer<'static> {
        self.cert_der.clone()
    }
}

#[cfg(test)]
pub fn generate_ca_for_tests() -> LoadedCa {
    generate_ca().expect("CA generation must succeed in tests")
}

/// Loads the CA from `dir`, generating it on first run. `dir` must survive a
/// reboot. Doesn't touch the system trust store (`platform::TrustStore::install`).
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
    // Root-only: this key can mint a cert for any name a trusting browser accepts.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on {}", key_path.display()))?;

    Ok(ca)
}

pub fn ca_cert_path(dir: &Path) -> PathBuf {
    dir.join(CA_CERT_FILE)
}

const TRUST_CERT_FILE: &str = "cert.pem";
const TRUST_BUNDLE_FILE: &str = "bundle.pem";

/// Writes `cert.pem` and `bundle.pem` (fghj's CA plus the host's roots) for
/// containers to mount. Separate from `ensure_ca`: the sidecar calls that on a
/// read-only mount.
pub fn refresh_trust_files(dir: &Path, ca: &LoadedCa) -> Result<()> {
    write_world_readable(&dir.join(TRUST_CERT_FILE), ca.cert_pem.as_bytes())?;
    let bundle = merge_bundle(&native_root_certs(), ca);
    write_world_readable(&dir.join(TRUST_BUNDLE_FILE), bundle.as_bytes())?;

    Ok(())
}

fn merge_bundle(roots: &[CertificateDer<'static>], ca: &LoadedCa) -> String {
    let mut bundle = String::new();
    for der in roots {
        // Already present once `platform::TrustStore::install` has run.
        if der == &ca.cert_der {
            continue;
        }
        let pem = pem::Pem::new("CERTIFICATE", der.to_vec());
        bundle.push_str(&pem::encode_config(
            &pem,
            pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF),
        ));
    }
    bundle.push_str(&ca.cert_pem);
    bundle
}

/// Best-effort: an empty or partly unparsable store still yields a usable bundle.
/// Enumerated as root on purpose: as the console user, macOS drops admin-domain
/// roots (including local MITM proxy CAs) and gains no user-domain ones.
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

const SIGNING_CERT_FILE: &str = "signing-cert.pem";
const SIGNING_KEY_FILE: &str = "signing-key.pem";
const SIGNING_GENERATION_FILE: &str = "signing-generation";

/// Never installed into a trust store, so it must not share `COMMON_NAME`:
/// `uninstall` deletes by name.
const SIGNING_COMMON_NAME: &str = "fghj signing CA";

/// Mirrors `dns::cert_eligible`. `dns::ZONE` is covered by `internal` but listed
/// in case that TLD is ever dropped.
fn permitted_dns_suffixes() -> Vec<&'static str> {
    let mut suffixes: Vec<&str> = std::iter::once(dns::ZONE)
        .chain(dns::RESERVED_ALIAS_TLDS.iter().copied())
        .collect();
    suffixes.sort_unstable();
    suffixes.dedup();
    suffixes
}

/// The IP exclusions matter: dNSName-only permitted subtrees leave IP SANs
/// unconstrained.
fn signing_name_constraints() -> NameConstraints {
    NameConstraints {
        permitted_subtrees: permitted_dns_suffixes()
            .into_iter()
            .map(|suffix| GeneralSubtree::DnsName(suffix.to_string()))
            .collect(),
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([0, 0, 0, 0], 0)),
            GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix([0; 16], 0)),
        ],
    }
}

/// Derived from the constraint set, so changing it regenerates the signing CA.
fn signing_generation() -> String {
    format!("v1 dns:{}", permitted_dns_suffixes().join(","))
}

/// The name-constrained subordinate CA that signs every leaf. Its key must be
/// world-readable for the sidecar (Docker's macOS bind mount checks permissions
/// host-side), so the constraints bound what a leak costs; the root key never
/// leaves `ca_dir()`. Constraining this CA rather than the root keeps the root,
/// and its trust approval, stable.
pub fn ensure_signing_ca(dir: &Path, root: &LoadedCa) -> Result<LoadedCa> {
    let cert_path = dir.join(SIGNING_CERT_FILE);
    let key_path = dir.join(SIGNING_KEY_FILE);
    let generation_path = dir.join(SIGNING_GENERATION_FILE);
    let wanted = signing_generation();

    let persisted = fs::read_to_string(&generation_path).unwrap_or_default();
    if cert_path.exists() && key_path.exists() && persisted.trim() == wanted {
        return load_ca(&cert_path, &key_path);
    }

    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let signing = generate_signing_ca(root)?;

    fs::write(&cert_path, &signing.cert_pem)
        .with_context(|| format!("failed to write {}", cert_path.display()))?;
    fs::write(&key_path, signing.key_pair.serialize_pem())
        .with_context(|| format!("failed to write {}", key_path.display()))?;
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on {}", key_path.display()))?;
    // Written last: a crash before this leaves the generation stale, which
    // regenerates.
    fs::write(&generation_path, &wanted)
        .with_context(|| format!("failed to write {}", generation_path.display()))?;

    Ok(signing)
}

pub fn signing_cert_path(dir: &Path) -> PathBuf {
    dir.join(SIGNING_CERT_FILE)
}

pub fn signing_key_path(dir: &Path) -> PathBuf {
    dir.join(SIGNING_KEY_FILE)
}

fn generate_signing_ca(root: &LoadedCa) -> Result<LoadedCa> {
    let key_pair = KeyPair::generate().context("failed to generate signing CA key pair")?;
    let mut params =
        CertificateParams::new(Vec::new()).context("failed to construct signing CA cert params")?;
    params
        .distinguished_name
        .push(DnType::CommonName, SIGNING_COMMON_NAME);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "fghj");
    // Leaves only.
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.use_authority_key_identifier_extension = true;
    params.name_constraints = Some(signing_name_constraints());

    let issuer = Issuer::from_ca_cert_der(&root.cert_der, &root.key_pair)
        .context("failed to build issuer from root CA certificate")?;
    let cert = params
        .signed_by(&key_pair, &issuer)
        .context("failed to sign the signing CA certificate")?;

    Ok(LoadedCa {
        key_pair,
        cert_pem: cert.pem(),
        cert_der: cert.der().clone(),
    })
}

/// Also what `uninstall` deletes by (`security delete-certificate -c`).
pub const COMMON_NAME: &str = "fghj local CA";

fn generate_ca() -> Result<LoadedCa> {
    let key_pair = KeyPair::generate().context("failed to generate CA key pair")?;
    let mut params =
        CertificateParams::new(Vec::new()).context("failed to construct CA cert params")?;
    params
        .distinguished_name
        .push(DnType::CommonName, COMMON_NAME);
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
    CertificateDer::from_pem_slice(pem.as_bytes()).context("failed to parse PEM certificate")
}

/// Mints a leaf per SNI on first use: wildcards only match one label. A
/// reserved-TLD alias only gets a cert while a running container routes it.
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
        // Strict OpenSSL rejects a leaf without AKI, SKI and `CA:FALSE`; rcgen only
        // writes the latter two for `ExplicitNoCa`.
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

        // Clients trust the root, so the chain must carry the signing CA.
        let cert_chain = vec![cert.der().clone(), self.ca_cert_der.clone()];
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let certified = CertifiedKey::from_der(cert_chain, key_der, &self.provider)
            .context("failed to build rustls CertifiedKey from issued leaf certificate")?;
        Ok(Arc::new(certified))
    }

    /// Split out for tests: rustls's `ClientHello` has no public constructor.
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

    struct StaticRoutes(std::collections::HashMap<&'static str, u16>);

    impl proxy::RouteResolver for StaticRoutes {
        fn resolve(&self, host: &str) -> Option<proxy::Backend> {
            self.0.get(host).copied().map(|port| proxy::Backend {
                host: "127.0.0.1".to_string(),
                port,
                traffic: None,
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

        use x509_parser::prelude::*;
        let (_, leaf) = X509Certificate::from_der(&leaf_der).unwrap();
        let (_, ca_cert) = X509Certificate::from_der(&ca_cert_der).unwrap();
        assert!(
            leaf.verify_signature(Some(ca_cert.public_key())).is_ok(),
            "leaf certificate signature must verify against the CA's public key"
        );
    }

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

    #[test]
    fn merge_bundle_lists_fghjs_ca_once_even_when_the_host_store_already_trusts_it() {
        let ca = generate_ca_for_tests();
        let other = generate_ca_for_tests();

        let store = vec![other.cert_der.clone(), ca.cert_der.clone()];
        let bundle = merge_bundle(&store, &ca);

        assert_eq!(
            bundle.matches(ca.cert_pem.trim()).count(),
            1,
            "fghj's CA must appear exactly once in the bundle, not twice"
        );
        assert!(
            bundle.contains(other.cert_pem.trim()),
            "de-duplicating our own CA must not drop anybody else's"
        );
        assert!(
            bundle.ends_with(&ca.cert_pem),
            "fghj's CA stays last, wherever the host store happened to list it"
        );
    }

    /// The signing CA must permit everything `dns::cert_eligible` mints for; a
    /// mismatch fails only at the client, with an error naming neither.
    #[test]
    fn the_permitted_suffixes_cover_every_name_the_resolver_will_issue_for() {
        let permitted = permitted_dns_suffixes();
        for tld in dns::RESERVED_ALIAS_TLDS {
            assert!(
                permitted.contains(tld),
                "reserved alias TLD {tld:?} is eligible for a cert but not permitted by the signing CA"
            );
        }
        assert!(
            permitted.iter().any(|s| dns::ZONE.ends_with(s)),
            "fghj's own zone ({}) must fall under some permitted subtree",
            dns::ZONE
        );
    }

    #[test]
    fn the_signing_ca_is_a_name_constrained_subordinate_of_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = generate_ca_for_tests();
        let signing = ensure_signing_ca(tmp.path(), &root).unwrap();

        use x509_parser::extensions::{GeneralName, ParsedExtension};
        use x509_parser::prelude::*;
        let (_, cert) = X509Certificate::from_der(&signing.cert_der).unwrap();
        let (_, root_cert) = X509Certificate::from_der(&root.cert_der).unwrap();

        assert!(
            cert.verify_signature(Some(root_cert.public_key())).is_ok(),
            "the signing CA must be signed by the root, not self-signed"
        );

        let bc = cert
            .basic_constraints()
            .unwrap()
            .expect("the signing CA must carry basicConstraints")
            .value;
        assert!(bc.ca, "the signing CA must be marked CA:TRUE");
        assert_eq!(
            bc.path_len_constraint,
            Some(0),
            "the signing CA must not be allowed to mint further sub-CAs"
        );

        let constraints = cert
            .iter_extensions()
            .find_map(|ext| match ext.parsed_extension() {
                ParsedExtension::NameConstraints(nc) => Some(nc),
                _ => None,
            })
            .expect("the signing CA must carry a nameConstraints extension");

        let mut permitted: Vec<&str> = constraints
            .permitted_subtrees
            .as_ref()
            .expect("nameConstraints must have permittedSubtrees")
            .iter()
            .map(|subtree| match subtree.base {
                GeneralName::DNSName(name) => name,
                ref other => panic!("unexpected permitted subtree kind: {other:?}"),
            })
            .collect();
        permitted.sort_unstable();
        assert_eq!(
            permitted,
            permitted_dns_suffixes(),
            "permittedSubtrees must be exactly the eligible DNS suffixes"
        );

        let excluded: Vec<usize> = constraints
            .excluded_subtrees
            .as_ref()
            .expect("nameConstraints must exclude the IP space")
            .iter()
            .map(|subtree| match subtree.base {
                GeneralName::IPAddress(bytes) => bytes.len(),
                ref other => panic!("unexpected excluded subtree kind: {other:?}"),
            })
            .collect();
        assert_eq!(
            excluded,
            vec![8, 32],
            "excludedSubtrees must be the whole IPv4 (4+4 bytes) and IPv6 (16+16) space"
        );
    }

    #[test]
    fn the_signing_ca_is_reloaded_until_the_constraint_set_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = generate_ca_for_tests();

        let first = ensure_signing_ca(tmp.path(), &root).unwrap();
        let second = ensure_signing_ca(tmp.path(), &root).unwrap();
        assert_eq!(
            first.cert_pem, second.cert_pem,
            "an unchanged constraint set must reload the persisted signing CA, not mint a new one"
        );

        fs::write(
            tmp.path().join(SIGNING_GENERATION_FILE),
            "v1 dns:something-else",
        )
        .unwrap();
        let third = ensure_signing_ca(tmp.path(), &root).unwrap();
        assert_ne!(
            first.cert_pem, third.cert_pem,
            "a changed constraint set must regenerate the signing CA"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join(SIGNING_GENERATION_FILE)).unwrap(),
            signing_generation(),
            "regenerating must leave the marker describing what was actually written"
        );
    }

    #[test]
    fn the_signing_cas_persisted_key_stays_root_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = generate_ca_for_tests();
        ensure_signing_ca(tmp.path(), &root).unwrap();

        let mode = fs::metadata(signing_key_path(tmp.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the copy in ca_dir() is root-only; only the sidecar's separate copy is world-readable"
        );
    }

    #[test]
    fn an_issued_leaf_is_served_with_its_issuer_and_chains_up_to_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = generate_ca_for_tests();
        let signing = ensure_signing_ca(tmp.path(), &root).unwrap();
        let signing_cert_der = signing.cert_der.clone();
        let resolver = DynamicCertResolver::new(signing, provider(), no_routes());

        let certified = resolver.resolve_for("cart.fghj.internal").unwrap();
        assert_eq!(
            certified.cert.len(),
            2,
            "the served chain must be leaf + issuing CA"
        );
        assert_eq!(
            certified.cert[1], signing_cert_der,
            "the second cert in the chain must be the signing CA"
        );

        use x509_parser::prelude::*;
        let (_, leaf) = X509Certificate::from_der(&certified.cert[0]).unwrap();
        let (_, intermediate) = X509Certificate::from_der(&certified.cert[1]).unwrap();
        let (_, root_cert) = X509Certificate::from_der(&root.cert_der).unwrap();
        assert!(
            leaf.verify_signature(Some(intermediate.public_key()))
                .is_ok(),
            "the leaf must verify against the signing CA"
        );
        assert!(
            intermediate
                .verify_signature(Some(root_cert.public_key()))
                .is_ok(),
            "the signing CA must verify against the root the OS actually trusts"
        );
    }

    /// Enforced, not just present. `issue` is called directly because `resolve_for`
    /// would refuse the out-of-zone name first.
    #[test]
    fn the_signing_cas_constraints_are_enforced_by_a_real_verifier() {
        use rustls::client::danger::ServerCertVerifier;

        let tmp = tempfile::tempdir().unwrap();
        let root = generate_ca_for_tests();
        let signing = ensure_signing_ca(tmp.path(), &root).unwrap();
        let resolver = DynamicCertResolver::new(signing, provider(), no_routes());

        let mut roots = rustls::RootCertStore::empty();
        roots.add(root.cert_der.clone()).unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider(),
        )
        .build()
        .unwrap();

        let verify = |name: &str| {
            let certified = resolver.issue(name).unwrap();
            let (leaf, intermediates) = certified.cert.split_first().unwrap();
            verifier.verify_server_cert(
                leaf,
                intermediates,
                &rustls_pki_types::ServerName::try_from(name.to_string()).unwrap(),
                &[],
                rustls_pki_types::UnixTime::now(),
            )
        };

        verify("cart.fghj.internal")
            .expect("an in-zone name must verify against the root through the signing CA");
        let out_of_zone = verify("login.microsoftonline.com").expect_err(
            "a name outside the permitted subtrees must be rejected even though fghj signed it",
        );
        assert!(
            matches!(out_of_zone, rustls::Error::InvalidCertificate(_)),
            "expected a certificate rejection, got {out_of_zone:?}"
        );
    }
}
