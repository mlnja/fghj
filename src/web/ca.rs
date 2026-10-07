//! The local certificate authority behind every `https://*.fghj.internal`
//! URL.
//!
//! Three jobs: generating/loading the CA itself (durable, under
//! `daemon::ca_dir` — regenerating it would mean re-approving it in Keychain
//! Access on every restart), minting leaf certs on demand as
//! [`DynamicCertResolver`] answers `web::proxy`'s TLS handshakes, and
//! installing trust — into the macOS system store for the browser, and as
//! mountable PEM files for containers that need to trust the zone from the
//! inside.
//!
//! Which names get a certificate is deliberately not "anything asked for":
//! see [`DynamicCertResolver::resolve_for`] and `dns::cert_eligible`.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::dns;
use crate::web::proxy;

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
/// `runs/`) can name the key file without reaching into `ca.rs` internals.
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
    let bundle = merge_bundle(&native_root_certs(), ca);
    write_world_readable(&dir.join(TRUST_BUNDLE_FILE), bundle.as_bytes())?;

    Ok(())
}

/// `roots` re-encoded as one PEM sequence with `ca`'s certificate appended.
/// Split out from `refresh_trust_files` only so the de-duplication below can
/// be tested without a host trust store that happens to contain fghj's CA.
fn merge_bundle(roots: &[CertificateDer<'static>], ca: &LoadedCa) -> String {
    let mut bundle = String::new();
    for der in roots {
        // Skip our own CA if the host store already has it — once
        // `install_macos_trust` has run, `load_native_certs` returns it as a
        // trusted root like any other, and appending it again below would put
        // it in `bundle.pem` twice. Harmless to a verifier, but it makes the
        // file lie about how many roots it carries, and it is the kind of
        // discrepancy that sends someone debugging the wrong thing.
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

/// This host's real root CA store (macOS Keychain / Linux system bundle /
/// ...). Best-effort by design: `rustls-native-certs` documents that a
/// handful of unparsable OS entries is normal, and even a wholly empty
/// result (e.g. a minimal container with no system store at all) should
/// still leave `bundle.pem` usable — just equivalent to `cert.pem` alone —
/// rather than failing the whole refresh.
///
/// Enumerated in-process, as root, which is worth stating because the
/// obvious objection to it is wrong. macOS trust settings live in three
/// domains — User, Admin, System — and `rustls-native-certs` reads all
/// three, but "User" means *the calling process's* user; `fghjd` is root, so
/// it reads root's own empty user domain. The apparent fix is to re-run the
/// enumeration as the console user. Measured on macOS 26, that makes the
/// bundle strictly worse: root's enumeration returned 163 roots, the console
/// user's 131, and the 32 missing included two locally-trusted MITM proxy
/// CAs — exactly the certificates whose absence breaks a container's
/// ordinary outbound HTTPS. Neither enumeration returns a user-domain-only
/// root at all (verified by fingerprint against a cert with
/// `SSL: kSecTrustSettingsResultTrustRoot` in the login keychain), so
/// dropping privileges buys nothing and costs the admin domain.
///
/// The user-domain gap is therefore real but not ours to close here: a CA
/// that only exists in someone's login keychain never reaches `bundle.pem`,
/// and the fix is to trust it in the admin/System domain — where everything
/// that prompts for a password already puts it — or to concatenate it into a
/// bundle of one's own.
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

/// The signing CA's Common Name. Never installed into any trust store — it
/// is trusted transitively, through the root, so it deliberately does not
/// share `COMMON_NAME` and cannot be caught by `uninstall`'s
/// `security delete-certificate -c`.
const SIGNING_COMMON_NAME: &str = "fghj signing CA";

/// Every DNS suffix the signing CA is permitted to issue under — exactly the
/// set `dns::cert_eligible` is willing to mint for, assembled from the same
/// two constants it reads so the two cannot drift apart.
///
/// `dns::ZONE` is already covered by the `internal` entry, and is listed
/// anyway: the cost is one redundant subtree, and the alternative is a
/// silent, total outage of fghj's own zone if that TLD is ever dropped from
/// the reserved list.
fn permitted_dns_suffixes() -> Vec<&'static str> {
    let mut suffixes: Vec<&str> = std::iter::once(dns::ZONE)
        .chain(dns::RESERVED_ALIAS_TLDS.iter().copied())
        .collect();
    suffixes.sort_unstable();
    suffixes.dedup();
    suffixes
}

/// X.509 `nameConstraints` for the signing CA.
///
/// `permittedSubtrees` of a dNSName constrains by label boundary, not string
/// prefix (RFC 5280 §4.2.1.10: a name satisfies the constraint if it can be
/// built by adding zero or more labels to the left), so `internal` permits
/// `internal` and `cart.fghj.internal` but not `notinternal`.
///
/// The excluded IP subtrees are not belt-and-braces. A `permittedSubtrees`
/// containing only dNSName entries leaves **every other name type wholly
/// unconstrained**, so without these a stolen signing key could still mint a
/// cert for an iPAddress SAN. `0.0.0.0/0` and `::/0` close that: nothing fghj
/// issues carries an IP SAN (`cert_eligible` only ever admits hostnames), so
/// excluding the whole space costs nothing.
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

/// A marker for *which* constraint set the persisted signing CA was built
/// with, derived from the set itself rather than hand-maintained alongside
/// it. Edit `dns::RESERVED_ALIAS_TLDS` and every existing install regenerates
/// its signing CA on the next start, with nothing to remember to bump.
fn signing_generation() -> String {
    format!("v1 dns:{}", permitted_dns_suffixes().join(","))
}

/// Loads (or mints and persists) the **signing** CA: a subordinate CA under
/// `root`, carrying the `nameConstraints` above, which is what actually signs
/// every leaf certificate.
///
/// The root exists only to sign this. That split is the whole point, and it
/// is about where the private keys end up rather than about PKI tidiness:
///
/// - The **root** key stays `0600` in `daemon::ca_dir()` and is never copied
///   anywhere. It is the key the OS trust store vouches for, so a leak of it
///   is a leak of trust for *every* hostname — `IsCa::Ca(Unconstrained)`
///   means a browser would accept a cert it signed for any domain at all.
/// - The **signing** key cannot stay that private. `runs::route_table`'s
///   `refresh_sidecar_ca_copy` has to hand it to every run's sidecar
///   container as a world-readable `0644` file, because Docker's macOS
///   bind-mount bridge performs its permission check host-side, as the
///   logged-in user, before the request reaches the container's UID
///   namespace — a `0600` root-owned file is simply unreadable through it.
///
/// So one of these two keys is, unavoidably, readable by any local process
/// and present inside containers. `nameConstraints` is what decides how much
/// that costs: with the root serving leaves directly, the answer was "every
/// site on the internet"; with this split it is "fghj's own zone and the
/// reserved TLDs".
///
/// Constraining a subordinate rather than the root is also what keeps the
/// decision reversible. Trust settings attach to the root, and that root's
/// bytes never change here — so widening the permitted set later (a real
/// custom domain over HTTPS, say) means minting a fresh signing CA under the
/// same root, which every client already trusts. Had the constraints gone on
/// the root itself, the same change would mean a new root and a fresh
/// Keychain Access approval on every machine, which is precisely the friction
/// `ensure_ca`'s durability exists to avoid.
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
    // `0600` for *this* copy. The sidecar's world-readable one is a separate
    // file written by `refresh_sidecar_ca_copy`, so the permission it needs
    // doesn't have to be the permission this one carries.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on {}", key_path.display()))?;
    // Written last: a crash between the cert and this marker leaves the
    // generation stale, which regenerates — the safe direction. The reverse
    // order could leave a fresh marker claiming constraints the persisted
    // cert doesn't actually carry.
    fs::write(&generation_path, &wanted)
        .with_context(|| format!("failed to write {}", generation_path.display()))?;

    Ok(signing)
}

/// Path to the signing CA's certificate under `dir` — exposed so
/// `runs::route_table` can copy it into the sidecar's own directory.
pub fn signing_cert_path(dir: &Path) -> PathBuf {
    dir.join(SIGNING_CERT_FILE)
}

/// Path to the signing CA's private key under `dir`. See `ensure_signing_ca`
/// for why this one, and not `ca_key_path`, is the key that leaves `ca_dir()`.
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
    // `Constrained(0)`, not `Unconstrained`: this CA may sign end-entity
    // certificates and nothing else. Without the path length, a stolen
    // signing key could mint further sub-CAs — still name-constrained, since
    // constraints are inherited down the chain, but there is no reason to
    // leave the depth open when fghj only ever issues leaves.
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    // Not self-signed, so RFC 5280 wants an AuthorityKeyIdentifier pointing
    // back at the root — the same requirement, and the same strict-OpenSSL
    // failure when it's missing, that `issue` documents for leaves.
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

/// The CA certificate's Common Name. Also the handle `uninstall` hands to
/// `security delete-certificate`, which matches on exactly this string —
/// so the generator and the remover cannot drift into naming different
/// certificates.
pub const COMMON_NAME: &str = "fghj local CA";

/// Machine-wide, not per-user: the CA has to be trusted for every browser
/// and every user on the box, and `fghjd` already runs as root.
pub const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

/// The inverse of [`install_macos_trust`] — deletes every System-keychain
/// certificate named [`COMMON_NAME`], returning how many it removed.
///
/// Loops rather than deleting once because `security delete-certificate`
/// removes a single match per invocation, and a machine that has run
/// several `fghjd` installs can hold several: deleting
/// `/var/lib/fghjd/ca/` makes the next start mint a *new* CA and trust it
/// too, leaving the old one behind. Uninstalling has to clear all of them
/// or it leaves trusted roots whose private keys the user thinks they
/// deleted.
///
/// A non-zero exit means "no certificate by that name", which is the
/// success condition here, not an error — so the loop ends on the first
/// failure and reports the count rather than propagating it.
pub fn remove_macos_trust() -> usize {
    if !cfg!(target_os = "macos") {
        return 0;
    }
    let mut removed = 0;
    // Bounded so a `security` that somehow always succeeds can't spin
    // forever; far above any plausible number of stale fghj CAs.
    while removed < 32 {
        let ok = Command::new("security")
            .args(["delete-certificate", "-c", COMMON_NAME, SYSTEM_KEYCHAIN])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            break;
        }
        removed += 1;
    }
    removed
}

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
/// invoke it. That promptlessness is also why `doctor` can report trust as
/// a read-only check rather than having to attempt the install to find out.
pub fn is_trusted_on_macos(ca_cert_path: &Path) -> bool {
    Command::new("security")
        .args(["verify-cert", "-c"])
        .arg(ca_cert_path)
        .args(["-k", SYSTEM_KEYCHAIN])
        // `output` rather than `status` purely to capture the subprocess's
        // own chatter: `verify-cert` prints "certificate verification
        // successful" on every call, and `doctor` calls this on demand, so
        // inheriting stdout would scatter that line through `fghjd`'s log.
        .output()
        .map(|o| o.status.success())
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
            SYSTEM_KEYCHAIN,
        ])
        .arg(ca_cert_path)
        .status()
        .context("failed to run `security add-trusted-cert`")?;
    if !status.success() {
        // Nearly always one specific thing: `SecTrustSettingsSetTrustSettings:
        // The authorization was denied since no user interaction was
        // possible.` Modifying System trust needs an Authorization Services
        // prompt, and a launchd *system* daemon has no session to show one in
        // — root does not bypass that gate. So the failure is not transient,
        // and with `KeepAlive` set the daemon would otherwise crash-loop on it
        // forever, logging a bare "failed" with nothing to act on. Hand over
        // the exact command instead: run from a terminal it can prompt, and
        // the check at the top of this function makes every later start a
        // no-op.
        anyhow::bail!(
            "`security add-trusted-cert` failed for {cert}\n\
             If this says the authorization was denied because no user interaction was \
             possible, fghjd is running without a session to prompt in (a launchd \
             LaunchDaemon, `brew services`, ssh). Install the trust once from a terminal:\n\
             \n\
             \x20   sudo security add-trusted-cert -d -r trustRoot -k {keychain} {cert}\n\
             \n\
             then start fghjd again.",
            cert = ca_cert_path.display(),
            keychain = SYSTEM_KEYCHAIN,
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

        // Leaf *and* its issuer. The issuer here is the name-constrained
        // signing CA (`ensure_signing_ca`), which no client has in its trust
        // store — clients trust the root, so the chain has to carry the
        // intermediate or verification fails with "unable to get local
        // issuer certificate". Sending it is also correct in the tests that
        // hand this resolver a self-signed root instead: a chain may include
        // its own trust anchor, and verifiers ignore the extra cert.
        let cert_chain = vec![cert.der().clone(), self.ca_cert_der.clone()];
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

    #[test]
    fn merge_bundle_lists_fghjs_ca_once_even_when_the_host_store_already_trusts_it() {
        let ca = generate_ca_for_tests();
        let other = generate_ca_for_tests();

        // What `load_native_certs` returns on a machine where
        // `install_macos_trust` has already run: fghj's own CA, sitting in the
        // host store as a trusted root like any other.
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

    /// The constraint set has to be a superset of what `dns::cert_eligible`
    /// will mint for. A name fghj is willing to issue for but the signing CA
    /// forbids fails verification at the *client*, with an error naming
    /// neither — so this is the test that catches someone adding a reserved
    /// TLD and forgetting the certificate side.
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

        // May sign leaves, and nothing below them.
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

        // Without these, a permittedSubtrees of only dNSName entries leaves
        // iPAddress wholly unconstrained — see `signing_name_constraints`.
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

        // Same thing `dns::RESERVED_ALIAS_TLDS` growing a TLD does to every
        // existing install: the marker no longer describes the persisted cert.
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

    /// Clients trust the root, not the signing CA, so a leaf served on its own
    /// fails with "unable to get local issuer certificate". The chain has to
    /// carry the intermediate.
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

    /// The constraints being *present* (above) is not the same as their being
    /// *enforced*. This runs the served chain through the same webpki path a
    /// rustls client uses, trusting only the root: an in-zone name must pass,
    /// and a name outside the permitted subtrees must be rejected even though
    /// the signature over it is perfectly valid.
    ///
    /// `issue` is called directly for the out-of-zone case on purpose —
    /// `resolve_for` would refuse the name first (`dns::cert_eligible`), which
    /// tests fghj's own gate rather than the certificate's.
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
